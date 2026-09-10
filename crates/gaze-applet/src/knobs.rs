//! Formatting for the tuning knobs: how many decimals a value shows, given its step.

use gaze_config::Knob;

/// Decimals a value should be shown with so that one step is visible: a step of 0.01
/// wants two, 0.5 wants one, 5 wants none.
pub fn decimals(step: f64) -> usize {
    let mut decimals = 0;
    let mut scaled   = step;

    // A step whose fractional part is not yet zero at this precision needs one more
    // digit. Capped so a strange step cannot ask for a screenful.
    while decimals < 4 && (scaled - scaled.round()).abs() > 1e-9 {
        scaled   *= 10.0;
        decimals += 1;
    }

    decimals
}

/// The value with the knob's decimals and unit, `"0.25 s"` or `"2"`.
pub fn format_value(knob: &Knob, value: f64) -> String {
    let number = format!("{:.*}", decimals(knob.step), value);

    if knob.unit.is_empty() {
        number
    }
    else {
        format!("{number} {}", knob.unit)
    }
}

/// The offset row: both angles signed to two decimals, then the counts.
pub fn format_offset(yaw_deg: f64, pitch_deg: f64, updates: u64, jumps: u64) -> String {
    format!("yaw {yaw_deg:+.2}°, pitch {pitch_deg:+.2}°, {updates} clicks, {jumps} jumps")
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_follow_the_step() {
        assert_eq!(decimals(5.0), 0);
        assert_eq!(decimals(1.0), 0);
        assert_eq!(decimals(0.5), 1);
        assert_eq!(decimals(0.1), 1);
        assert_eq!(decimals(0.05), 2);
        assert_eq!(decimals(0.01), 2);
        assert_eq!(decimals(0.005), 3);
    }

    #[test]
    fn values_carry_their_unit_only_when_there_is_one() {
        let seconds = Knob { key: "k", label: "K", unit: "s", min: 0.0, max: 1.0, step: 0.05 };
        let bare    = Knob { key: "k", label: "K", unit: "",  min: 0.0, max: 1.0, step: 1.0  };

        assert_eq!(format_value(&seconds, 0.25), "0.25 s");
        assert_eq!(format_value(&bare, 2.0), "2");
    }

    #[test]
    fn the_offset_row_signs_both_angles() {
        assert_eq!(
            format_offset(0.5, -1.25, 12, 1),
            "yaw +0.50°, pitch -1.25°, 12 clicks, 1 jumps",
        );
    }
}
