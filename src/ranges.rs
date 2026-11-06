//! Body ranges: text styles and mentions, in signal-cli's `start:length:VALUE` form.
//! Positions count UTF-16 code units, as Signal's clients do.

use presage::proto::BodyRange;
use presage::proto::body_range::{AssociatedValue, Style};

pub fn style_name(style: Style) -> Option<&'static str> {
    Some(match style {
        Style::Bold => "BOLD",
        Style::Italic => "ITALIC",
        Style::Spoiler => "SPOILER",
        Style::Strikethrough => "STRIKETHROUGH",
        Style::Monospace => "MONOSPACE",
        Style::None => return None,
    })
}

fn parse_style(name: &str) -> Option<Style> {
    Some(match name.to_ascii_uppercase().as_str() {
        "BOLD" => Style::Bold,
        "ITALIC" => Style::Italic,
        "SPOILER" => Style::Spoiler,
        "STRIKETHROUGH" => Style::Strikethrough,
        "MONOSPACE" => Style::Monospace,
        _ => return None,
    })
}

/// Splits `start:length:value`. The value may itself contain colons (`PNI:<uuid>`).
pub fn split(range: &str) -> Result<(u32, u32, &str), String> {
    let mut parts = range.splitn(3, ':');
    let (Some(start), Some(length), Some(value)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("expected start:length:value, got {range:?}"));
    };
    let start = start
        .trim()
        .parse()
        .map_err(|_| format!("bad start in {range:?}"))?;
    let length = length
        .trim()
        .parse()
        .map_err(|_| format!("bad length in {range:?}"))?;
    Ok((start, length, value.trim()))
}

pub fn style_range(range: &str) -> Result<BodyRange, String> {
    let (start, length, name) = split(range)?;
    let style = parse_style(name).ok_or_else(|| format!("unknown text style {name:?}"))?;
    Ok(BodyRange {
        start: Some(start),
        length: Some(length),
        associated_value: Some(AssociatedValue::Style(style.into())),
    })
}

pub fn mention_range(
    start: u32,
    length: u32,
    aci: presage::libsignal_service::prelude::Uuid,
) -> BodyRange {
    BodyRange {
        start: Some(start),
        length: Some(length),
        associated_value: Some(AssociatedValue::MentionAci(aci.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_style_ranges() {
        let r = style_range("0:4:BOLD").unwrap();
        assert_eq!((r.start, r.length), (Some(0), Some(4)));
        assert_eq!(
            r.associated_value,
            Some(AssociatedValue::Style(Style::Bold.into()))
        );
        assert!(style_range("0:4:SPARKLY").is_err());
        assert!(style_range("0:BOLD").is_err());
    }

    #[test]
    fn keeps_colons_in_the_value() {
        let (s, l, v) = split("3:1:PNI:0000").unwrap();
        assert_eq!((s, l, v), (3, 1, "PNI:0000"));
    }
}
