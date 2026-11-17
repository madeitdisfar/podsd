//! What Conversation Awareness's levels (04 00 04 00 4B 00 02 00 01 LL) ask
//! of the volume here. A level is where the volume should be, not an event:
//! the AirPods hold one as long as it stands and send nothing more, so the
//! volume follows the last one, with no timer.

/// What a level asks for. A depth is how much of the whole lowering
/// (Lowering) applies, in dB: 1 all of it, 0 none.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// The user is speaking: lower to this depth, at once, starting a
    /// conversation if none is
    Lower(f64),
    /// Part way back, within a conversation already started: this depth
    Partial(f64),
    /// Back to the whole volume; the conversation is over
    End,
    /// Nothing for the volume
    Ignore,
}

pub fn step(level: u8) -> Step {
    match level {
        0x01 | 0x02 => Step::Lower(1.0),
        0x03 => Step::Partial(0.5),
        0x05 => Step::Partial(0.25),
        0x04 | 0x06..=0x09 => Step::End,
        _ => Step::Ignore,
    }
}

/// How far a conversation lowers the volume: by `cut_db`, and to no more
/// than `ceiling` percent, since the people talking are as loud whatever the
/// music's volume (LibrePods' Android app lowers to a ceiling too)
#[derive(Debug, Clone, Copy)]
pub struct Lowering {
    pub cut_db: f64,
    pub ceiling: u32,
}

impl Default for Lowering {
    fn default() -> Self {
        Lowering { cut_db: 15.0, ceiling: 35 }
    }
}

/// A sink's volume percent is on PulseAudio's cubic scale: 50% is -18 dB
fn to_db(percent: u32) -> f64 {
    60.0 * (percent as f64 / 100.0).log10()
}

fn from_db(db: f64) -> u32 {
    (100.0 * 10f64.powf(db / 60.0)).round() as u32
}

/// The volume for `depth` of the lowering, from `original`; never above it
pub fn target(original: u32, depth: f64, lowering: Lowering) -> u32 {
    if original == 0 {
        return 0;
    }
    let from = to_db(original);
    let full = (from - lowering.cut_db).min(to_db(lowering.ceiling.max(1))).min(from);
    from_db(from + depth.clamp(0.0, 1.0) * (full - from)).min(original)
}

/// The sink's volume reads back rounded to a percent: a gap past that is
/// the user's (or another app's) doing, and theirs stands
pub fn changed_by_someone_else(current: u32, applied: u32) -> bool {
    current.abs_diff(applied) > 2
}

/// Raising goes in this many steps, FADE_STEP apart (0.75 s)
pub const FADE_STEPS: u32 = 10;
pub const FADE_STEP: std::time::Duration = std::time::Duration::from_millis(75);

/// The next volume on the way from `from` up to `to`, moving `stride` at a time
pub fn next_up(from: u32, to: u32, stride: u32) -> u32 {
    from.saturating_add(stride.max(1)).min(to)
}

pub fn stride(from: u32, to: u32) -> u32 {
    to.saturating_sub(from).div_ceil(FADE_STEPS).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_levels_seen_map_to_targets() {
        assert_eq!(step(1), Step::Lower(1.0));
        assert_eq!(step(2), Step::Lower(1.0));
        assert_eq!(step(3), Step::Partial(0.5));
        assert_eq!(step(5), Step::Partial(0.25));
        for level in [4, 6, 7, 8, 9] {
            assert_eq!(step(level), Step::End, "level {level}");
        }
        assert_eq!(step(0x0b), Step::Ignore);
        assert_eq!(step(0), Step::Ignore);
    }

    #[test]
    fn the_cut_applies_in_db_up_to_the_ceiling() {
        let lowering = Lowering::default();
        // 54% is -16 dB: 15 dB down is 30%, under the ceiling
        assert_eq!(target(54, 1.0, lowering), 30);
        assert_eq!(target(54, 0.5, lowering), 40);
        assert_eq!(target(54, 0.0, lowering), 54);
        // Loud: the ceiling holds it at 35%
        assert_eq!(target(100, 1.0, lowering), 35);
        // Quiet: still the 15 dB
        assert_eq!(target(25, 1.0, lowering), 14);
        assert_eq!(target(0, 1.0, lowering), 0);
        // A ceiling above the volume never raises it
        assert_eq!(target(20, 1.0, Lowering { cut_db: 0.0, ceiling: 35 }), 20);
    }

    #[test]
    fn a_fade_reaches_its_target_in_ten_steps_or_fewer() {
        let (from, to) = (11, 54);
        let by = stride(from, to);
        let mut volume = from;
        let mut steps = 0;
        while volume != to {
            volume = next_up(volume, to, by);
            steps += 1;
        }
        assert!(steps <= FADE_STEPS, "{steps} steps");
        assert_eq!(next_up(50, 52, stride(50, 52)), 51);
    }

    #[test]
    fn rounding_is_not_taken_for_the_user() {
        assert!(!changed_by_someone_else(25, 24));
        assert!(!changed_by_someone_else(25, 27));
        assert!(changed_by_someone_else(40, 25));
    }
}
