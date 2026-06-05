pub const BLUE: &str = "\x1b[38;2;0;153;255m";
pub const ORANGE: &str = "\x1b[38;2;255;176;85m";
pub const GREEN: &str = "\x1b[38;2;0;160;0m";
pub const CYAN: &str = "\x1b[38;2;46;149;153m";
pub const RED: &str = "\x1b[38;2;255;85;85m";
pub const YELLOW: &str = "\x1b[38;2;230;200;0m";
pub const PURPLE: &str = "\x1b[38;2;167;139;250m";
pub const WHITE: &str = "\x1b[38;2;220;220;220m";
pub const DIM: &str = "\x1b[2m";
pub const RESET: &str = "\x1b[0m";

pub fn usage_color(pct: i64) -> &'static str {
    match pct {
        p if p >= 90 => RED,
        p if p >= 70 => ORANGE,
        p if p >= 50 => YELLOW,
        _ => GREEN,
    }
}
