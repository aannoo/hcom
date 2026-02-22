use ratatui::style::{Modifier, Style};

/// Tokyo Night inspired palette
pub mod palette {
    use ratatui::style::Color;

    // Base
    pub const FG: Color = Color::Rgb(192, 202, 245);
    pub const FG_MID: Color = Color::Rgb(140, 148, 190);
    pub const FG_DIM: Color = Color::Rgb(86, 95, 137);
    pub const FG_DARK: Color = Color::Rgb(59, 66, 97);

    // Accent
    pub const BLUE: Color = Color::Rgb(122, 162, 247);
    pub const CYAN: Color = Color::Rgb(125, 207, 255);
    pub const GREEN: Color = Color::Rgb(158, 206, 106);
    pub const TEAL: Color = Color::Rgb(80, 180, 150);
    pub const RED: Color = Color::Rgb(247, 118, 142);
    pub const YELLOW: Color = Color::Rgb(224, 175, 104);
    pub const ORANGE: Color = Color::Rgb(255, 158, 100);
    pub const MAGENTA: Color = Color::Rgb(187, 154, 247);

    // Surface
    pub const SELECTION: Color = Color::Rgb(40, 44, 67);
    pub const SEARCH_BG: Color = Color::Rgb(60, 55, 30);

    // Mode-tinted input backgrounds
    pub const MODE_SEARCH: Color = Color::Rgb(45, 42, 25);
    pub const MODE_CMD: Color = Color::Rgb(42, 35, 55);
    pub const MODE_TAG: Color = Color::Rgb(50, 38, 25);
    pub const MODE_COMPOSE: Color = Color::Rgb(52, 57, 83);
}

pub struct Theme;

impl Theme {
    // Status
    pub fn active() -> Style {
        Style::default().fg(palette::GREEN)
    }
    pub fn listening() -> Style {
        Style::default().fg(palette::CYAN)
    }
    pub fn blocked() -> Style {
        Style::default().fg(palette::RED)
    }
    pub fn launching() -> Style {
        Style::default().fg(palette::YELLOW)
    }
    pub fn inactive() -> Style {
        Style::default().fg(palette::FG_DIM)
    }

    // UI chrome
    pub fn title() -> Style {
        Style::default()
            .fg(palette::BLUE)
            .add_modifier(Modifier::BOLD)
    }
    pub fn dim() -> Style {
        Style::default().fg(palette::FG_DIM)
    }
    pub fn separator() -> Style {
        Style::default().fg(palette::FG_DARK)
    }
    pub fn cursor() -> Style {
        Style::default().fg(palette::BLUE)
    }
    pub fn mention() -> Style {
        Style::default()
            .fg(palette::ORANGE)
            .add_modifier(Modifier::BOLD)
    }
    pub fn delivery() -> Style {
        Style::default().fg(palette::GREEN)
    }

    // Search
    pub fn search_match() -> Style {
        Style::default()
            .fg(palette::FG)
            .bg(palette::SEARCH_BG)
            .add_modifier(Modifier::BOLD)
    }

    // Flash
    pub fn flash_ok() -> Style {
        Style::default()
            .fg(palette::GREEN)
            .add_modifier(Modifier::BOLD)
    }
    pub fn flash_info() -> Style {
        Style::default()
            .fg(palette::BLUE)
            .add_modifier(Modifier::BOLD)
    }
    pub fn flash_err() -> Style {
        Style::default()
            .fg(palette::RED)
            .add_modifier(Modifier::BOLD)
    }

    // PTY delivery gate-blocked indicator
    pub fn pty_block() -> Style {
        Style::default().fg(palette::ORANGE)
    }

    // Agent
    pub fn agent_name() -> Style {
        Style::default()
            .fg(palette::FG)
            .add_modifier(Modifier::BOLD)
    }
    pub fn agent_context() -> Style {
        Style::default().fg(palette::FG_MID)
    }
    pub fn agent_dim() -> Style {
        Style::default().fg(palette::FG_DIM)
    }

    // Launch
    pub fn launch_active() -> Style {
        Style::default().fg(palette::BLUE)
    }
    pub fn launch_arrow() -> Style {
        Style::default().fg(palette::FG_DIM)
    }
}
