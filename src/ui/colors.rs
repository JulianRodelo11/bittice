use ratatui::style::Color;

pub const BLUE: Color = Color::Rgb(137, 180, 249);
pub const PURPLE: Color = Color::Rgb(197, 137, 249);
pub const MUTED_PURPLE: Color = Color::Rgb(115, 115, 130);
pub const GREEN: Color = Color::Rgb(149, 213, 178);
pub const PINK: Color = Color::Rgb(255, 179, 193);
pub const SAND: Color = Color::Rgb(248, 225, 175);
pub const PURPLE_SECONDARY: Color = Color::Rgb(184, 184, 255);

// Semantic aliases
pub const PRIMARY_COLOR: Color = BLUE;
pub const MUTED_COLOR: Color = MUTED_PURPLE;
pub const ACTIVE_COLOR: Color = BLUE;
pub const INACTIVE_COLOR: Color = MUTED_PURPLE;
pub const ADD_COLOR: Color = GREEN;
pub const DELETE_COLOR: Color = PINK;
pub const KEY_COLOR: Color = Color::DarkGray;
pub const VALUE_COLOR: Color = Color::Cyan;
pub const DISABLED_COLOR: Color = Color::DarkGray;
pub const PLACEHOLDER_COLOR: Color = Color::DarkGray;
pub const INSTRUCTION_COLOR: Color = SAND;
pub const SELECTED_BORDER_COLOR: Color = PURPLE_SECONDARY;
