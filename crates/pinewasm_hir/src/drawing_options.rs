//! Serialized string values for the drawing constants, and the option domains the renderer accepts.
//! Each value is the compact string used for that drawing constant in this crate.
//! The canonical member names remain the schema-v1 drawing DTO vocabulary.
use crate::{CellAttr, DrawKind};

pub const DRAWING_STRING_CONSTANTS: &[(&str, &str)] = &[
    ("position.top_left", "top_left"),
    ("position.top_center", "top_center"),
    ("position.top_right", "top_right"),
    ("position.middle_left", "middle_left"),
    ("position.middle_center", "middle_center"),
    ("position.middle_right", "middle_right"),
    ("position.bottom_left", "bottom_left"),
    ("position.bottom_center", "bottom_center"),
    ("position.bottom_right", "bottom_right"),
    ("size.auto", "auto"),
    ("size.tiny", "tiny"),
    ("size.small", "small"),
    ("size.normal", "normal"),
    ("size.large", "large"),
    ("size.huge", "huge"),
    ("xloc.bar_index", "bi"),
    ("xloc.bar_time", "bt"),
    ("yloc.price", "pr"),
    ("yloc.abovebar", "ab"),
    ("yloc.belowbar", "bl"),
    ("extend.none", "n"),
    ("extend.left", "l"),
    ("extend.right", "r"),
    ("extend.both", "b"),
    ("line.style_solid", "sol"),
    ("line.style_dotted", "dot"),
    ("line.style_dashed", "dsh"),
    ("line.style_arrow_left", "al"),
    ("line.style_arrow_right", "ar"),
    ("line.style_arrow_both", "ab"),
    ("label.style_none", "n"),
    ("label.style_xcross", "xcr"),
    ("label.style_cross", "cr"),
    ("label.style_triangleup", "tup"),
    ("label.style_triangledown", "tdn"),
    ("label.style_flag", "flg"),
    ("label.style_circle", "cir"),
    ("label.style_arrowup", "aup"),
    ("label.style_arrowdown", "adn"),
    ("label.style_label_up", "lup"),
    ("label.style_label_down", "ldn"),
    ("label.style_label_left", "llf"),
    ("label.style_label_right", "lrg"),
    ("label.style_label_lower_left", "llwlf"),
    ("label.style_label_lower_right", "llwrg"),
    ("label.style_label_upper_left", "luplf"),
    ("label.style_label_upper_right", "luprg"),
    ("label.style_label_center", "lcn"),
    ("label.style_square", "sq"),
    ("label.style_diamond", "dia"),
    ("label.style_text_outline", "to"),
    ("text.align_left", "left"),
    ("text.align_center", "center"),
    ("text.align_right", "right"),
    ("text.align_top", "top"),
    ("text.align_bottom", "bottom"),
    ("text.wrap_none", "none"),
    ("text.wrap_auto", "auto"),
    ("font.family_default", "default"),
    ("font.family_monospace", "monospace"),
];

pub fn pine_drawing_string(name: &str) -> Option<&'static str> {
    DRAWING_STRING_CONSTANTS
        .iter()
        .find_map(|&(member, value)| (member == name).then_some(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrawingOptionDomain {
    XLoc,
    YLoc,
    Extension,
    LineStyle,
    LabelStyle,
    Size,
    HorizontalAlign,
    VerticalAlign,
    Wrap,
    Font,
    Position,
}

impl DrawingOptionDomain {
    pub fn members(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        DRAWING_STRING_CONSTANTS
            .iter()
            .copied()
            .filter(move |(member, _)| match self {
                Self::XLoc => member.starts_with("xloc."),
                Self::YLoc => member.starts_with("yloc."),
                Self::Extension => member.starts_with("extend."),
                Self::LineStyle => member.starts_with("line."),
                Self::LabelStyle => member.starts_with("label."),
                Self::Size => member.starts_with("size."),
                Self::HorizontalAlign => matches!(
                    *member,
                    "text.align_left" | "text.align_center" | "text.align_right"
                ),
                Self::VerticalAlign => matches!(
                    *member,
                    "text.align_top" | "text.align_center" | "text.align_bottom"
                ),
                Self::Wrap => member.starts_with("text.wrap_"),
                Self::Font => member.starts_with("font."),
                Self::Position => member.starts_with("position."),
            })
    }

    /// Accept canonical DTO values and the Pine string aliases listed for this
    /// domain only. Unknown values remain unknown to the caller.
    pub fn canonical_value(self, value: &str) -> Option<&'static str> {
        self.members().find_map(|(member, pine_value)| {
            (value == member || value == pine_value).then_some(member)
        })
    }
}

pub fn drawing_option_domain(kind: DrawKind, field: &str) -> Option<DrawingOptionDomain> {
    use DrawingOptionDomain as D;
    match (kind, field) {
        (DrawKind::Line | DrawKind::Label | DrawKind::Box | DrawKind::Polyline, "xloc") => {
            Some(D::XLoc)
        }
        (DrawKind::Label, "yloc") => Some(D::YLoc),
        (DrawKind::Line | DrawKind::Box, "extend") => Some(D::Extension),
        (DrawKind::Line, "style")
        | (DrawKind::Box, "border_style")
        | (DrawKind::Polyline, "line_style") => Some(D::LineStyle),
        (DrawKind::Label, "style") => Some(D::LabelStyle),
        (DrawKind::Label, "size") | (DrawKind::Box, "text_size") => Some(D::Size),
        (DrawKind::Label, "textalign") | (DrawKind::Box, "text_halign") => Some(D::HorizontalAlign),
        (DrawKind::Box, "text_valign") => Some(D::VerticalAlign),
        (DrawKind::Box, "text_wrap") => Some(D::Wrap),
        (DrawKind::Label | DrawKind::Box, "text_font_family") => Some(D::Font),
        (DrawKind::Table, "position") => Some(D::Position),
        _ => None,
    }
}

pub fn cell_option_domain(attr: CellAttr) -> Option<DrawingOptionDomain> {
    use DrawingOptionDomain as D;
    match attr {
        CellAttr::TextHalign => Some(D::HorizontalAlign),
        CellAttr::TextValign => Some(D::VerticalAlign),
        CellAttr::TextSize => Some(D::Size),
        CellAttr::TextFontFamily => Some(D::Font),
        _ => None,
    }
}
