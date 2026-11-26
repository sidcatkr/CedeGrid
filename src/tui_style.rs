//! The one look of CedeGrid's terminal screens, `setup` and `top` (003 FR-030, FR-033): bold
//! titles and the highlighted row, yellow for caution, red for refusals, dim hints. Without color
//! (`NO_COLOR`, or output that is not a terminal) every style is the terminal's default, so no
//! color attribute is ever set.

use ratatui::style::{Color, Modifier, Style};

/// The styles of one screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub color: bool,
}

impl Palette {
    fn when(self, style: Style) -> Style {
        if self.color { style } else { Style::default() }
    }

    /// A screen or section title.
    pub fn title(self) -> Style {
        self.when(Style::default().add_modifier(Modifier::BOLD))
    }

    /// The highlighted row.
    pub fn selected(self) -> Style {
        self.when(Style::default().add_modifier(Modifier::BOLD | Modifier::REVERSED))
    }

    /// A caution option and its marker.
    pub fn caution(self) -> Style {
        self.when(Style::default().fg(Color::Yellow))
    }

    /// A refusal or an error.
    pub fn refusal(self) -> Style {
        self.when(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    }

    /// Hints and other quiet text.
    pub fn dim(self) -> Style {
        self.when(Style::default().add_modifier(Modifier::DIM))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_color_every_style_is_the_default() {
        let plain = Palette { color: false };
        for style in [
            plain.title(),
            plain.selected(),
            plain.caution(),
            plain.refusal(),
            plain.dim(),
        ] {
            assert_eq!(style, Style::default());
        }
        assert_eq!(Palette { color: true }.caution().fg, Some(Color::Yellow));
    }
}
