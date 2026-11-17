//! What Conversation Awareness's levels (04 00 04 00 4B 00 02 00 01 LL) ask
//! of the volume here. A level is where the volume should be, not an event:
//! the AirPods hold one as long as it stands and send nothing more, so the
//! volume follows the last one, with no timer.

/// What a level asks for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The user is speaking: lower to this share of the volume, at once,
    /// starting a conversation if none is
    Lower(u32),
    /// Part way back, within a conversation already started: this share
    Partial(u32),
    /// Back to the whole volume; the conversation is over
    End,
    /// Nothing for the volume
    Ignore,
}

pub fn step(level: u8) -> Step {
    match level {
        0x01 => Step::Lower(25),
        0x02 => Step::Lower(20),
        0x03 => Step::Partial(40),
        0x05 => Step::Partial(60),
        0x04 | 0x06..=0x09 => Step::End,
        _ => Step::Ignore,
    }
}

/// `share` percent of `volume`
pub fn scaled(volume: u32, share: u32) -> u32 {
    (volume * share + 50) / 100
}

/// The sink's volume reads back rounded to a percent: a gap past that is
/// the user's (or another app's) doing, and theirs stands
pub fn changed_by_someone_else(current: u32, applied: u32) -> bool {
    current.abs_diff(applied) > 2
}

/// Raising goes in this many steps, FADE_STEP apart (about 1.5 s)
pub const FADE_STEPS: u32 = 10;
pub const FADE_STEP: std::time::Duration = std::time::Duration::from_millis(150);

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
        assert_eq!(step(1), Step::Lower(25));
        assert_eq!(step(2), Step::Lower(20));
        assert_eq!(step(3), Step::Partial(40));
        assert_eq!(step(5), Step::Partial(60));
        for level in [4, 6, 7, 8, 9] {
            assert_eq!(step(level), Step::End, "level {level}");
        }
        assert_eq!(step(0x0b), Step::Ignore);
        assert_eq!(step(0), Step::Ignore);
    }

    #[test]
    fn shares_round_to_the_nearest_percent() {
        assert_eq!(scaled(54, 25), 14);
        assert_eq!(scaled(54, 60), 32);
        assert_eq!(scaled(0, 40), 0);
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
