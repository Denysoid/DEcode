use std::sync::OnceLock;

use ratatui::{style::Color, text::Span};
use tui_syntax_highlight::{
    Highlighter,
    syntect::{
        highlighting::{Theme, ThemeSet},
        parsing::SyntaxSet,
        util::LinesWithEndings,
    },
};

struct SyntaxAssets {
    syntaxes: SyntaxSet,
    theme: Option<Theme>,
}

static ASSETS: OnceLock<SyntaxAssets> = OnceLock::new();

fn assets() -> &'static SyntaxAssets {
    ASSETS.get_or_init(|| {
        let themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| themes.themes.values().next().cloned());
        SyntaxAssets {
            syntaxes: SyntaxSet::load_defaults_newlines(),
            theme,
        }
    })
}

#[must_use]
pub fn highlight_source(path: &str, source: &str) -> Option<Vec<Vec<Span<'static>>>> {
    let assets = assets();
    let theme = assets.theme.as_ref()?;
    let syntax = std::path::Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(|extension| assets.syntaxes.find_syntax_by_extension(extension))
        .unwrap_or_else(|| assets.syntaxes.find_syntax_plain_text());
    let text = Highlighter::new(theme.clone())
        .override_background(Color::Reset)
        .line_numbers(false)
        .highlight_lines(LinesWithEndings::from(source), syntax, &assets.syntaxes)
        .ok()?;
    Some(text.lines.into_iter().map(|line| line.spans).collect())
}

#[cfg(test)]
mod tests {
    use super::highlight_source;

    #[test]
    fn rust_source_is_highlighted_without_line_numbers() {
        let lines = highlight_source("src/main.rs", "fn main() {}\n");
        assert!(
            lines.is_some(),
            "built-in syntax assets must include a usable theme"
        );
        let lines = lines.unwrap_or_default();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].iter().any(|span| !span.content.is_empty()));
        assert!(lines[0].iter().all(|span| !span.content.contains('│')));
    }

    #[test]
    fn syntax_extension_matching_is_case_insensitive() {
        let source = "fn main() { println!(\"hello\"); }\n";

        let lowercase = highlight_source("src/main.rs", source);
        let uppercase = highlight_source("src/MAIN.RS", source);

        assert_eq!(uppercase, lowercase);
    }
}
