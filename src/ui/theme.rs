//! Palette (REQ-007 AC-007-20; D-42/ADR-010 local TUI theme — NOT read from
//! the remote ui-theme settings namespace).
//!
//! Semantic roles → ratatui Color. `dark`/`light` pick a built-in table;
//! `[ui].palette` overrides individual roles (hex `#rrggbb` or a CSS-ish
//! color name). Invalid role value / unknown role → role default + warning
//! (AC-007-21 same-source semantics), never a crash.

use ratatui::style::Color;

/// Semantic palette roles used across the UI (role list is stable; renderers
/// resolve through `Palette::color(role)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Role {
    // (Ord 由 derive 按声明序生成；无依赖外部排序)
    /// Normal foreground (default text).
    Normal,
    /// Accent (mode indicator, active highlights).
    Accent,
    /// Selection (list cursor / selected row).
    Selection,
    /// Error.
    Error,
    /// Warning.
    Warn,
    /// User-message prefix color.
    UserFg,
    /// Assistant-message prefix color.
    AssistantFg,
    /// Tool call/result prefix color.
    ToolFg,
    /// Borders.
    Border,
    /// Code / syntax fallback accent.
    Code,
}

impl Role {
    pub const ALL: [Role; 10] = [
        Role::Normal,
        Role::Accent,
        Role::Selection,
        Role::Error,
        Role::Warn,
        Role::UserFg,
        Role::AssistantFg,
        Role::ToolFg,
        Role::Border,
        Role::Code,
    ];

    /// Config key (role → override).
    pub fn key(self) -> &'static str {
        match self {
            Role::Normal => "normal",
            Role::Accent => "accent",
            Role::Selection => "selection",
            Role::Error => "error",
            Role::Warn => "warn",
            Role::UserFg => "user_fg",
            Role::AssistantFg => "assistant_fg",
            Role::ToolFg => "tool_fg",
            Role::Border => "border",
            Role::Code => "code",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|r| r.key() == key)
    }
}

/// Dark built-in table (default; role colors mirror the pre-V0.4 hardcoded
/// UI values so default rendering is unchanged).
pub fn dark_builtin(role: Role) -> Color {
    match role {
        Role::Normal => Color::Reset,
        Role::Accent => Color::Cyan,
        Role::Selection => Color::LightBlue,
        Role::Error => Color::Red,
        Role::Warn => Color::Yellow,
        Role::UserFg => Color::Cyan,
        Role::AssistantFg => Color::Green,
        Role::ToolFg => Color::Yellow,
        Role::Border => Color::DarkGray,
        Role::Code => Color::Blue,
    }
}

/// Light built-in table (V0.4; default fg flipped to dark text on light bg).
pub fn light_builtin(role: Role) -> Color {
    match role {
        Role::Normal => Color::Reset,
        Role::Accent => Color::Blue,
        Role::Selection => Color::Blue,
        Role::Error => Color::Red,
        Role::Warn => Color::Yellow,
        Role::UserFg => Color::Blue,
        Role::AssistantFg => Color::Green,
        Role::ToolFg => Color::Yellow,
        Role::Border => Color::Gray,
        Role::Code => Color::Blue,
    }
}

/// Effective palette: builtin table + user overrides.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    /// "dark" | "light" (builtin selection).
    pub theme: String,
    /// Role → resolved Color (already merged with builtin; unknown-role /
    /// invalid-value overrides fall back to the builtin and record a warning).
    pub resolved: std::collections::BTreeMap<Role, Color>,
    /// Warnings from override resolution (surfaced once at startup; never
    /// fatal).
    pub warnings: Vec<String>,
}

impl Default for Palette {
    fn default() -> Self {
        Self::build("dark", &std::collections::BTreeMap::new())
    }
}

impl Palette {
    /// Build from config theme + palette overrides.
    pub fn build(theme: &str, overrides: &std::collections::BTreeMap<String, String>) -> Self {
        let builtin = if theme == "light" {
            light_builtin
        } else {
            dark_builtin
        };
        let mut resolved = std::collections::BTreeMap::new();
        let mut warnings = Vec::new();
        for role in Role::ALL {
            let fallback = builtin(role);
            let mut color = fallback;
            if let Some(raw) = overrides.get(role.key()) {
                match parse_color(raw) {
                    Some(c) => color = c,
                    None => warnings.push(format!(
                        "未知/非法 palette 颜色值：{}={:?}（回退 {} 默认色）",
                        role.key(),
                        raw,
                        theme
                    )),
                }
            }
            resolved.insert(role, color);
        }
        // 覆盖表里未知角色保留但警告（配置项不丢弃；UI 永不崩溃）。
        for key in overrides.keys() {
            if Role::from_key(key).is_none() {
                warnings.push(format!("未知 palette 角色：{key:?}（忽略）"));
            }
        }
        Palette {
            theme: theme.to_string(),
            resolved,
            warnings,
        }
    }

    /// Resolve one role to a concrete Color.
    pub fn color(&self, role: Role) -> Color {
        self.resolved
            .get(&role)
            .copied()
            .unwrap_or_else(|| dark_builtin(role))
    }

    pub fn is_light(&self) -> bool {
        self.theme == "light"
    }
}

/// Parse a color spec: `#rrggbb` / `#rgb` hex, or a small set of color names
/// (ratatui Color::* strings). Returns None for invalid input.
pub fn parse_color(raw: &str) -> Option<Color> {
    let t = raw.trim();
    if let Some(hex) = t.strip_prefix('#') {
        return parse_hex(hex);
    }
    match t.to_ascii_lowercase().as_str() {
        "reset" | "default" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "dark_gray" => Some(Color::DarkGray),
        "lightred" | "light_red" => Some(Color::LightRed),
        "lightgreen" | "light_green" => Some(Color::LightGreen),
        "lightyellow" | "light_yellow" => Some(Color::LightYellow),
        "lightblue" | "light_blue" => Some(Color::LightBlue),
        "lightmagenta" | "light_magenta" => Some(Color::LightMagenta),
        "lightcyan" | "light_cyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

/// Parse `#rgb` / `#rrggbb` hex.
fn parse_hex(hex: &str) -> Option<Color> {
    let h = hex.trim();
    if h.len() != 6 && h.len() != 3 {
        return None;
    }
    let digits = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok();
    if h.len() == 3 {
        let (r, g, b) = (digits(0)?, digits(1)?, digits(2)?);
        return Some(Color::Rgb(r * 17, g * 17, b * 17));
    }
    let rd = u8::from_str_radix(&h[0..2], 16).ok()?;
    let gd = u8::from_str_radix(&h[2..4], 16).ok()?;
    let bd = u8::from_str_radix(&h[4..6], 16).ok()?;
    Some(Color::Rgb(rd, gd, bd))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_dark_and_light_differ_on_accent() {
        assert_eq!(dark_builtin(Role::Accent), Color::Cyan);
        assert_eq!(light_builtin(Role::Accent), Color::Blue);
    }

    #[test]
    fn build_applies_overrides_and_falls_back_on_invalid() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("accent".to_string(), "#ff0000".to_string());
        overrides.insert("error".to_string(), "not-a-color".to_string());
        overrides.insert("future_role".to_string(), "#000000".to_string());
        let p = Palette::build("dark", &overrides);
        assert_eq!(p.color(Role::Accent), Color::Rgb(255, 0, 0));
        assert_eq!(p.color(Role::Error), Color::Red, "非法值回退默认");
        assert_eq!(p.color(Role::Normal), Color::Reset);
        assert!(p.warnings.iter().any(|w| w.contains("not-a-color")));
        assert!(p.warnings.iter().any(|w| w.contains("future_role")));
    }

    #[test]
    fn parse_color_accepts_hex_and_names() {
        assert_eq!(parse_color("#ff0000"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#f00"), Some(Color::Rgb(255, 0, 0)));
        assert_eq!(parse_color("#123456"), Some(Color::Rgb(0x12, 0x34, 0x56)));
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("LightBlue"), Some(Color::LightBlue));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("zzz"), None);
        assert_eq!(parse_color("#gggggg"), None);
    }

    #[test]
    fn light_theme_flag() {
        let p = Palette::build("light", &std::collections::BTreeMap::new());
        assert!(p.is_light());
        let p = Palette::build("dark", &std::collections::BTreeMap::new());
        assert!(!p.is_light());
        assert!(p.warnings.is_empty());
    }
}
