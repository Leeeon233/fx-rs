use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub background: Color,
    pub surface: Color,
    pub surface_high: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub secondary: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
}

impl Theme {
    #[must_use]
    pub fn detect() -> Self {
        if std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            return Self::mono();
        }
        let true_color = std::env::var("COLORTERM").is_ok_and(|value| {
            value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit")
        }) || std::env::var("TERM").is_ok_and(|value| value.contains("direct"))
            || std::env::var("TERM_PROGRAM").is_ok_and(|value| {
                matches!(
                    value.as_str(),
                    "Apple_Terminal" | "Ghostty" | "Hyper" | "WezTerm" | "iTerm.app"
                )
            });
        if true_color {
            Self::dark()
        } else {
            Self::ansi256()
        }
    }

    #[must_use]
    pub const fn dark() -> Self {
        Self {
            background: Color::Rgb(12, 14, 18),
            surface: Color::Rgb(21, 24, 31),
            surface_high: Color::Rgb(31, 35, 45),
            text: Color::Rgb(224, 228, 238),
            muted: Color::Rgb(124, 132, 151),
            accent: Color::Rgb(82, 214, 214),
            secondary: Color::Rgb(176, 135, 255),
            success: Color::Rgb(91, 214, 142),
            warning: Color::Rgb(245, 190, 78),
            danger: Color::Rgb(244, 105, 118),
        }
    }

    const fn mono() -> Self {
        Self {
            background: Color::Reset,
            surface: Color::Reset,
            surface_high: Color::Reset,
            text: Color::White,
            muted: Color::DarkGray,
            accent: Color::White,
            secondary: Color::Gray,
            success: Color::White,
            warning: Color::White,
            danger: Color::White,
        }
    }

    const fn ansi256() -> Self {
        Self {
            background: Color::Indexed(234),
            surface: Color::Indexed(235),
            surface_high: Color::Indexed(238),
            text: Color::Indexed(252),
            muted: Color::Indexed(244),
            accent: Color::Indexed(80),
            secondary: Color::Indexed(141),
            success: Color::Indexed(78),
            warning: Color::Indexed(221),
            danger: Color::Indexed(204),
        }
    }
}
