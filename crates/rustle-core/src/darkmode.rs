//! Dark-mode adaptation of message HTML, in the spirit of Outlook's reader:
//! light backgrounds are pulled down to dark, dark text and borders are lifted
//! to light, and everything keeps its hue so brand colours survive. Nothing
//! else is touched -- images, already-dark designs, mid-tone colours.
//!
//! The reader runs with JavaScript off, so this is a source rewrite: inline
//! `style` attributes, the legacy colour attributes (`bgcolor`, `color`,
//! `text`, …) and `<style>` blocks. Layout is never altered.

use std::sync::LazyLock;

use regex::{Captures, Regex};

/// The canvas the reader paints behind a message in dark mode, and the text
/// colour an unstyled body gets. `adapt` maps white onto the same lightness.
pub const CANVAS: &str = "#1a1a1a";
pub const TEXT: &str = "#f0f0f0";

/// Rewrite `html` for a dark canvas.
pub fn adapt(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() + html.len() / 8);
    let mut pos = 0;
    while let Some(open) = lower[pos..].find('<') {
        let open = pos + open;
        out.push_str(&html[pos..open]);
        let after = &lower[open + 1..];
        if after.starts_with("!--") {
            let end = after.find("-->").map_or(html.len(), |e| open + 1 + e + 3);
            out.push_str(&html[open..end]);
            pos = end;
            continue;
        }
        let is_tag = after
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '/' || c == '!');
        let Some(close) = tag_end(&html[open..]).filter(|_| is_tag) else {
            out.push('<');
            pos = open + 1;
            continue;
        };
        let close = open + close;
        let tag = &html[open..=close];
        out.push_str(&rewrite_tag(tag));
        pos = close + 1;
        if after.starts_with("style")
            && !after[5..].starts_with(|c: char| c.is_ascii_alphanumeric())
        {
            let end = lower[pos..].find("</style").map_or(html.len(), |e| pos + e);
            out.push_str(&rewrite_stylesheet(&html[pos..end]));
            pos = end;
        }
    }
    out.push_str(&html[pos..]);
    out
}

/// Index of the `>` closing the tag that starts at `tag[0]`, honouring quotes.
fn tag_end(tag: &str) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &b) in tag.as_bytes().iter().enumerate().skip(1) {
        match quote {
            Some(q) if b == q => quote = None,
            Some(_) => {}
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'>' => return Some(i),
            None => {}
        }
    }
    None
}

static ATTRIBUTE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)(\s(style|bgcolor|color|text|link|alink|vlink|bordercolor)\s*=\s*)(?:"([^"]*)"|'([^']*)'|([^\s"'>]+))"#,
    )
    .expect("attribute pattern is valid")
});

fn rewrite_tag(tag: &str) -> String {
    ATTRIBUTE
        .replace_all(tag, |caps: &Captures| {
            let name = caps[2].to_ascii_lowercase();
            let (value, quote) = match (caps.get(3), caps.get(4), caps.get(5)) {
                (Some(v), _, _) => (v.as_str(), "\""),
                (_, Some(v), _) => (v.as_str(), "'"),
                (_, _, Some(v)) => (v.as_str(), "\""),
                _ => unreachable!(),
            };
            let new = match name.as_str() {
                "style" => rewrite_declarations(value),
                "bgcolor" => rewrite_colours(value, Role::Background),
                "bordercolor" => rewrite_colours(value, Role::Border),
                _ => rewrite_colours(value, Role::Text),
            };
            format!("{}{quote}{new}{quote}", &caps[1])
        })
        .into_owned()
}

static RULE_BODY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([^{}]*)\}").expect("rule pattern is valid"));

fn rewrite_stylesheet(css: &str) -> String {
    RULE_BODY
        .replace_all(css, |caps: &Captures| {
            format!("{{{}}}", rewrite_declarations(&caps[1]))
        })
        .into_owned()
}

/// Rewrite a `prop: value; prop: value` list, leaving unknown properties as
/// they are.
fn rewrite_declarations(style: &str) -> String {
    split_declarations(style)
        .into_iter()
        .map(|declaration| {
            let Some((property, value)) = declaration.split_once(':') else {
                return declaration.to_string();
            };
            let Some(role) = Role::of(property.trim()) else {
                return declaration.to_string();
            };
            format!("{property}:{}", rewrite_colours(value, role))
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Split on `;` outside parentheses, so a `url(data:…;base64,…)` survives.
fn split_declarations(style: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, c) in style.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth = (depth - 1).max(0),
            ';' if depth == 0 => {
                parts.push(&style[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&style[start..]);
    parts
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Role {
    Background,
    Text,
    Border,
}

impl Role {
    fn of(property: &str) -> Option<Self> {
        let property = property.to_ascii_lowercase();
        Some(match property.as_str() {
            "background" | "background-color" => Role::Background,
            "color" | "-webkit-text-fill-color" | "fill" => Role::Text,
            p if p.starts_with("border") || p.starts_with("outline") => Role::Border,
            "box-shadow" | "text-decoration-color" | "column-rule-color" => Role::Border,
            _ => return None,
        })
    }
}

static COLOUR_TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)url\([^)]*\)|[a-z-]+-gradient\([^)]*\)|#[0-9a-f]{3,8}\b|rgba?\([^)]*\)|hsla?\([^)]*\)|[a-z]+",
    )
    .expect("colour token pattern is valid")
});

fn rewrite_colours(value: &str, role: Role) -> String {
    COLOUR_TOKEN
        .replace_all(value, |caps: &Captures| {
            let token = &caps[0];
            match parse(token) {
                Some(colour) => adapt_colour(colour, role)
                    .map(|c| c.to_css())
                    .unwrap_or_else(|| token.to_string()),
                None => token.to_string(),
            }
        })
        .into_owned()
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rgba {
    r: f64,
    g: f64,
    b: f64,
    a: f64,
}

impl Rgba {
    fn from_u8(r: u8, g: u8, b: u8) -> Self {
        Rgba {
            r: f64::from(r) / 255.0,
            g: f64::from(g) / 255.0,
            b: f64::from(b) / 255.0,
            a: 1.0,
        }
    }

    /// Rec. 601 luma: how light the colour reads.
    fn luma(&self) -> f64 {
        0.299 * self.r + 0.587 * self.g + 0.114 * self.b
    }

    fn to_hsl(self) -> (f64, f64, f64) {
        let max = self.r.max(self.g).max(self.b);
        let min = self.r.min(self.g).min(self.b);
        let l = (max + min) / 2.0;
        if max == min {
            return (0.0, 0.0, l);
        }
        let d = max - min;
        let s = if l > 0.5 {
            d / (2.0 - max - min)
        } else {
            d / (max + min)
        };
        let h = if max == self.r {
            (self.g - self.b) / d + if self.g < self.b { 6.0 } else { 0.0 }
        } else if max == self.g {
            (self.b - self.r) / d + 2.0
        } else {
            (self.r - self.g) / d + 4.0
        } / 6.0;
        (h, s, l)
    }

    fn from_hsl(h: f64, s: f64, l: f64, a: f64) -> Self {
        if s == 0.0 {
            return Rgba {
                r: l,
                g: l,
                b: l,
                a,
            };
        }
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let channel = |mut t: f64| {
            if t < 0.0 {
                t += 1.0;
            }
            if t > 1.0 {
                t -= 1.0;
            }
            if t < 1.0 / 6.0 {
                p + (q - p) * 6.0 * t
            } else if t < 0.5 {
                q
            } else if t < 2.0 / 3.0 {
                p + (q - p) * (2.0 / 3.0 - t) * 6.0
            } else {
                p
            }
        };
        Rgba {
            r: channel(h + 1.0 / 3.0),
            g: channel(h),
            b: channel(h - 1.0 / 3.0),
            a,
        }
    }

    fn to_css(self) -> String {
        let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        if self.a >= 1.0 {
            format!(
                "#{:02x}{:02x}{:02x}",
                byte(self.r),
                byte(self.g),
                byte(self.b)
            )
        } else {
            format!(
                "rgba({},{},{},{})",
                byte(self.r),
                byte(self.g),
                byte(self.b),
                self.a
            )
        }
    }
}

/// Map a colour for its role, or `None` when it is fine as it is.
fn adapt_colour(colour: Rgba, role: Role) -> Option<Rgba> {
    if colour.a <= 0.0 {
        return None;
    }
    let luma = colour.luma();
    let (h, s, _) = colour.to_hsl();
    let lightness = match role {
        // Light canvases go dark: white lands on CANVAS, and a pale tint or
        // a highlight keeps its hue at a deep shade.
        Role::Background if luma > 0.5 => 0.10 + (1.0 - luma) * 0.35,
        // Dark ink goes light: black becomes TEXT, a dark brand colour a
        // pastel of itself.
        Role::Text if luma < 0.5 => 0.94 - luma * 0.6,
        // Rules and frames lift too, but stay quieter than text.
        Role::Border if luma < 0.5 => 0.75 - luma * 0.5,
        _ => return None,
    };
    Some(Rgba::from_hsl(h, s, lightness, colour.a))
}

fn parse(token: &str) -> Option<Rgba> {
    let token = token.trim();
    if let Some(hex) = token.strip_prefix('#') {
        return parse_hex(hex);
    }
    let lower = token.to_ascii_lowercase();
    if let Some(args) = lower
        .strip_prefix("rgba(")
        .or_else(|| lower.strip_prefix("rgb("))
    {
        return parse_rgb(args.trim_end_matches(')'));
    }
    if let Some(args) = lower
        .strip_prefix("hsla(")
        .or_else(|| lower.strip_prefix("hsl("))
    {
        return parse_hsl(args.trim_end_matches(')'));
    }
    // Legacy system colours Word and Outlook write into their markup.
    match lower.as_str() {
        "windowtext" => return Some(Rgba::from_u8(0, 0, 0)),
        "window" => return Some(Rgba::from_u8(255, 255, 255)),
        _ => {}
    }
    NAMED
        .binary_search_by(|(name, _)| name.cmp(&lower.as_str()))
        .ok()
        .map(|i| {
            let v = NAMED[i].1;
            Rgba::from_u8((v >> 16) as u8, (v >> 8) as u8, v as u8)
        })
}

fn parse_hex(hex: &str) -> Option<Rgba> {
    let digit = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let bytes = hex.as_bytes();
    let (r, g, b, a) = match bytes.len() {
        3 | 4 => {
            let d: Vec<u8> = bytes.iter().map(|&c| digit(c)).collect::<Option<_>>()?;
            let a = d.get(3).map_or(255, |&x| x * 17);
            (d[0] * 17, d[1] * 17, d[2] * 17, a)
        }
        6 | 8 => {
            let d: Vec<u8> = bytes
                .chunks(2)
                .map(|p| Some(digit(p[0])? * 16 + digit(p[1])?))
                .collect::<Option<_>>()?;
            (d[0], d[1], d[2], d.get(3).copied().unwrap_or(255))
        }
        _ => return None,
    };
    Some(Rgba {
        a: f64::from(a) / 255.0,
        ..Rgba::from_u8(r, g, b)
    })
}

fn parse_rgb(args: &str) -> Option<Rgba> {
    let parts: Vec<&str> = args
        .split([',', '/', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let channel = |s: &str| -> Option<f64> {
        if let Some(pct) = s.strip_suffix('%') {
            pct.parse::<f64>().ok().map(|p| p / 100.0)
        } else {
            s.parse::<f64>().ok().map(|v| v / 255.0)
        }
    };
    let alpha = |s: &str| -> Option<f64> {
        if let Some(pct) = s.strip_suffix('%') {
            pct.parse::<f64>().ok().map(|p| p / 100.0)
        } else {
            s.parse::<f64>().ok()
        }
    };
    Some(Rgba {
        r: channel(parts[0])?,
        g: channel(parts[1])?,
        b: channel(parts[2])?,
        a: parts.get(3).map_or(Some(1.0), |s| alpha(s))?,
    })
}

fn parse_hsl(args: &str) -> Option<Rgba> {
    let parts: Vec<&str> = args
        .split([',', '/', ' '])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let h = parts[0]
        .trim_end_matches("deg")
        .parse::<f64>()
        .ok()?
        .rem_euclid(360.0)
        / 360.0;
    let pct = |s: &str| s.strip_suffix('%')?.parse::<f64>().ok().map(|p| p / 100.0);
    let a = match parts.get(3) {
        Some(s) => pct(s).or_else(|| s.parse().ok())?,
        None => 1.0,
    };
    Some(Rgba::from_hsl(h, pct(parts[1])?, pct(parts[2])?, a))
}

/// CSS named colours, sorted for binary search.
static NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_table_is_sorted() {
        assert!(NAMED.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn white_canvas_and_black_ink_flip() {
        let html = r##"<body bgcolor="#FFFFFF"><p style="color:#000; background-color: white">x</p></body>"##;
        assert_eq!(
            adapt(html),
            format!(
                r##"<body bgcolor="{CANVAS}"><p style="color:{TEXT}; background-color: {CANVAS}">x</p></body>"##
            )
        );
    }

    #[test]
    fn dark_designs_and_mid_tones_are_left_alone() {
        let html =
            r##"<td style="background:#000000;color:#8bc34a;border:1px solid #6495ed">x</td>"##;
        assert_eq!(adapt(html), html);
    }

    #[test]
    fn highlights_keep_their_hue() {
        let out = adapt(r#"<span style="background: yellow">x</span>"#);
        let hex = out
            .split("background: ")
            .nth(1)
            .unwrap()
            .trim_end_matches("\">x</span>");
        let c = parse(hex).unwrap();
        let (h, s, l) = c.to_hsl();
        assert!((h - 1.0 / 6.0).abs() < 0.01, "{hex}");
        assert!(s > 0.9 && l < 0.2, "{hex}");
    }

    #[test]
    fn stylesheets_urls_and_comments_survive() {
        let html = "<style>@media all { p { color: black; background: url(x.png) #fff } }</style><!-- <p style=\"color:#000\"> --><div style=\"background:url(data:image/png;base64,AA;BB) #eee\">";
        let out = adapt(html);
        assert!(
            out.contains(&format!("color: {TEXT}; background: url(x.png) {CANVAS}")),
            "{out}"
        );
        assert!(out.contains("<!-- <p style=\"color:#000\"> -->"));
        assert!(
            out.contains("url(data:image/png;base64,AA;BB) #1f1f1f"),
            "{out}"
        );
    }

    #[test]
    fn alpha_and_functional_notation() {
        let out = adapt(r#"<p style="color: rgba(0, 0, 0, 0.5); border-color: rgb(51,51,51)">"#);
        assert!(out.contains("color: rgba(240,240,240,0.5)"), "{out}");
        assert!(out.contains("border-color: #a6a6a6"), "{out}");
        let out = adapt("<td style=\"border-color: windowtext; background: window\">");
        assert!(
            out.contains("border-color: #bfbfbf; background: #1a1a1a"),
            "{out}"
        );
        assert!(!adapt("<p style=\"color: transparent; margin: 0\">").contains('#'));
    }
}
