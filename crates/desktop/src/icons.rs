use rune::Icon;

const fn mark(name: &'static str, svg: &'static str) -> Icon {
    Icon { name, view: 100.0, body: svg }
}

static CLAUDE: Icon = mark("switch-claude", include_str!("../assets/claude.svg"));
static ANTIGRAVITY: Icon = mark("switch-antigravity", include_str!("../assets/antigravity.svg"));

pub fn provider_mark(provider: &str) -> Option<&'static Icon> {
    match provider {
        "claude" => Some(&CLAUDE),
        "antigravity" => Some(&ANTIGRAVITY),
        _ => None,
    }
}

const fn outline(name: &'static str, body: &'static str) -> Icon {
    Icon { name, view: 24.0, body }
}

pub static SWITCH: Icon = outline(
    "switch-mark",
    r#"<path d="M9 5.5H15A6.5 6.5 0 0 1 15 18.5H9A6.5 6.5 0 0 1 9 5.5Z" stroke="currentColor" stroke-width="2" fill="none"/><path d="M15 9A3 3 0 1 1 15 15A3 3 0 1 1 15 9Z" fill="currentColor"/>"#,
);
pub static GRIP: Icon = outline(
    "switch-grip",
    r#"<path d="M9 6h.01M15 6h.01M9 12h.01M15 12h.01M9 18h.01M15 18h.01" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round"/>"#,
);
pub static BACK: Icon = outline(
    "switch-back",
    r#"<path d="M15 18L9 12L15 6" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static OPEN: Icon = outline(
    "switch-open",
    r#"<path d="M9 18L15 12L9 6" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static REFRESH: Icon = outline(
    "switch-refresh",
    r#"<path d="M21 12a9 9 0 1 1-2.64-6.36L21 8M21 3v5h-5" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static SPINNER: Icon = outline(
    "switch-spinner",
    r#"<path d="M21 12a9 9 0 1 1-6.2-8.56" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round"/>"#,
);
pub static CHECK: Icon = outline(
    "switch-check",
    r#"<path d="M20 6L9 17L4 12" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static UP_DOWN: Icon = outline(
    "switch-up-down",
    r#"<path d="M7 15L12 20L17 15M7 9L12 4L17 9" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static WARNING: Icon = outline(
    "switch-warning",
    r#"<path d="M12 9v4M12 17h.01M10.3 3.9L1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0z" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static MORE: Icon = outline(
    "switch-more",
    r#"<path d="M5 12h.01M12 12h.01M19 12h.01" stroke="currentColor" stroke-width="3" fill="none" stroke-linecap="round"/>"#,
);
pub static CLOSE: Icon = outline(
    "switch-close",
    r#"<path d="M18 6L6 18M6 6l12 12" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round"/>"#,
);
pub static CHEVRON_DOWN: Icon = outline(
    "switch-chevron-down",
    r#"<path d="M6 9L12 15L18 9" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
pub static CHEVRON_UP: Icon = outline(
    "switch-chevron-up",
    r#"<path d="M18 15L12 9L6 15" stroke="currentColor" stroke-width="2" fill="none" stroke-linecap="round" stroke-linejoin="round"/>"#,
);
