use ratatui::style::Color;

pub const BLUE: Color = Color::Rgb(136, 190, 245);
pub const PURPLE: Color = Color::Rgb(200, 136, 221);
pub const MUTED_PURPLE: Color = Color::Rgb(204, 204, 221);
pub const GREEN: Color = Color::Rgb(203, 239, 176);
pub const PINK: Color = Color::Rgb(252, 165, 165);
pub const SAND: Color = Color::Rgb(222, 185, 133);
pub const PURPLE_SECONDARY: Color = Color::Rgb(219, 192, 254);

// Semantic aliases
pub const PRIMARY_COLOR: Color = PURPLE_SECONDARY;
pub const MUTED_COLOR: Color = Color::DarkGray;
pub const ACTIVE_COLOR: Color = PURPLE_SECONDARY;
pub const INACTIVE_COLOR: Color = Color::DarkGray;
pub const ADD_COLOR: Color = GREEN;
pub const DELETE_COLOR: Color = PINK;
pub const KEY_COLOR: Color = Color::DarkGray;
pub const VALUE_COLOR: Color = Color::Cyan;
pub const DISABLED_COLOR: Color = Color::DarkGray;
pub const PLACEHOLDER_COLOR: Color = Color::DarkGray;
pub const INSTRUCTION_COLOR: Color = SAND;
pub const SELECTED_BORDER_COLOR: Color = Color::White;
