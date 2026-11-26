use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use jiff::Timestamp;
use opentui::prelude::*;

use crate::connection::Update;
use crate::format::truncate;
use crate::model::{Action, Connection, Model, Overlay, Response, Target};
use crate::rows::{Row, dashboard, help, reset_picker};

const CLOCK_INTERVAL: Duration = Duration::from_secs(15);
const PROMPT_LABEL_MAX: usize = 30;
const DASHBOARD_HINTS: [(&str, &str); 6] =
    [("enter", "switch"), ("r", "refresh"), ("s", "auto"), ("t", "resets"), ("?", "help"), ("q", "quit")];
const HELP_HINTS: [(&str, &str); 3] = [("esc", "close"), ("up/down", "scroll"), ("q", "quit")];
const RENAME_HINTS: [(&str, &str); 2] = [("enter", "save"), ("esc", "cancel")];
const REMOVE_HINTS: [(&str, &str); 2] = [("y", "remove"), ("esc", "cancel")];
const RESET_PICK_HINTS: [(&str, &str); 3] = [("up/down", "select"), ("enter", "use"), ("esc", "cancel")];
const RESET_CONFIRM_HINTS: [(&str, &str); 2] = [("y", "reset"), ("esc", "cancel")];

pub struct Screen {
    app: App,
    model: RefCell<Model>,
    row_targets: RefCell<Vec<Option<Target>>>,
    shown_overlay: RefCell<Overlay>,
    viewport: Cell<(f32, f32)>,
    now: Box<dyn Fn() -> Timestamp>,
    run_action: Box<dyn Fn(Action)>,
    header: Text,
    list: ScrollBox,
    body: Text,
    status: Text,
    rename: View,
    alias: Input,
    footer: View,
}

impl Screen {
    pub fn build(app: &App, now: impl Fn() -> Timestamp + 'static, run_action: impl Fn(Action) + 'static) -> Rc<Self> {
        let header = Text::new(app, "");
        let body = Text::new(app, "").wrap(WrapMode::None).selectable(false);
        let list = ScrollBox::new(app).axes(false, true).focusable(false).grow(1.0).child(&body);
        let status = Text::new(app, "").min_height(1).max_height(3);
        let alias = Input::new(app).placeholder("empty clears the alias").grow(1.0);
        let rename = View::new(app).row().child(&Text::new(app, "Alias: ")).child(&alias).visible(false);
        let footer = View::new(app).row().flex_wrap(Wrap::Wrap).column_gap(2).shrink(0.0);
        let title = View::new(app)
            .row()
            .justify(Justify::SpaceBetween)
            .column_gap(2)
            .child(&Text::new(app, "Switch").bold())
            .child(&header);
        let bottom = View::new(app).padding_x(1).shrink(0.0).child(&status).child(&rename).child(&footer);
        app.root().add(&title.padding_x(1).margin_edge(Edge::Bottom, 1).shrink(0.0)).add(&list).add(&bottom);

        let screen = Rc::new(Self {
            app: app.clone(),
            model: RefCell::new(Model::new()),
            row_targets: RefCell::default(),
            shown_overlay: RefCell::new(Overlay::None),
            viewport: Cell::default(),
            now: Box::new(now),
            run_action: Box::new(run_action),
            header,
            list,
            body,
            status,
            rename,
            alias,
            footer,
        });
        screen.handle_events();
        screen.set_hints(&DASHBOARD_HINTS);
        screen.draw();
        screen
    }

    pub fn apply(&self, update: &Update) {
        self.model.borrow_mut().apply(update);
        self.draw();
        if matches!(update, Update::Snapshot(_)) {
            self.reveal_selection();
        }
    }

    fn handle_events(self: &Rc<Self>) {
        let screen = Rc::clone(self);
        self.app.on_key(move |_, key| {
            let response = screen.model.borrow_mut().press(key);
            screen.respond(response);
        });
        let screen = Rc::clone(self);
        self.alias.on_submit(move |_, alias| {
            let response = screen.model.borrow_mut().submit_alias(alias);
            screen.respond(response);
        });
        let screen = Rc::clone(self);
        self.list.on_click(move |_, event| screen.click(event));
        let screen = Rc::clone(self);
        self.list.on_resize(move |_, rect| {
            if screen.viewport.replace((rect.width, rect.height)) != (rect.width, rect.height) {
                let screen = Rc::clone(&screen);
                screen.app.clone().set_timeout(Duration::ZERO, move |_| {
                    screen.draw();
                    screen.reveal_selection();
                });
            }
        });
        let screen = Rc::clone(self);
        self.app.set_interval(CLOCK_INTERVAL, move |_| screen.draw());
    }

    fn respond(&self, response: Response) {
        match response {
            Response::Ignored => return,
            Response::Quit => return self.app.quit(),
            Response::ScrollHelp(rows) => {
                self.list.scroll_by(0.0, rows);
                return;
            }
            Response::Run(action) => (self.run_action)(action),
            Response::Changed => {}
        }
        self.draw();
        self.reveal_selection();
    }

    fn click(&self, event: &MouseEvent) {
        if self.model.borrow().overlay != Overlay::None {
            return;
        }
        let row = f64::from(event.y) - self.body.rect().screen_y;
        let target = self.row_targets.borrow().get(row.max(0.0) as usize).cloned().flatten();
        if let Some(target) = target {
            self.model.borrow_mut().select(target);
            self.draw();
        }
    }

    fn draw(&self) {
        let model = self.model.borrow();
        self.draw_overlay(&model.overlay);
        self.header.set_text(self.header_text(&model));
        self.status.set_text(self.status_text(&model));
        let rows = match &model.overlay {
            Overlay::Help => help(),
            Overlay::Reset { label, grants, index, confirming: false, .. } => reset_picker(
                label,
                grants,
                *index,
                &self.app.theme(),
                (self.now)(),
                &jiff::tz::TimeZone::system(),
                self.content_width(),
            ),
            _ => {
                dashboard(&model, &self.app.theme(), (self.now)(), &jiff::tz::TimeZone::system(), self.content_width())
            }
        };
        let (targets, lines): (Vec<_>, Vec<_>) = rows.into_iter().map(|Row { target, spans }| (target, spans)).unzip();
        self.body.set_text(join_lines(lines));
        *self.row_targets.borrow_mut() = targets;
    }

    fn draw_overlay(&self, overlay: &Overlay) {
        if *self.shown_overlay.borrow() == *overlay {
            return;
        }
        let renaming = matches!(overlay, Overlay::Rename { .. });
        self.rename.visible(renaming);
        self.status.visible(!renaming);
        match overlay {
            Overlay::Rename { alias, .. } => {
                self.alias.set_value(alias);
                self.alias.focus();
                self.set_hints(&RENAME_HINTS);
            }
            Overlay::Remove { .. } => self.set_hints(&REMOVE_HINTS),
            Overlay::Reset { confirming: false, .. } => self.set_hints(&RESET_PICK_HINTS),
            Overlay::Reset { confirming: true, .. } => self.set_hints(&RESET_CONFIRM_HINTS),
            Overlay::Help => {
                self.list.scroll_to(0.0, 0.0);
                self.set_hints(&HELP_HINTS);
            }
            Overlay::None => {
                self.app.blur();
                self.set_hints(&DASHBOARD_HINTS);
            }
        }
        *self.shown_overlay.borrow_mut() = overlay.clone();
    }

    fn set_hints(&self, hints: &[(&'static str, &'static str)]) {
        self.footer.clear();
        for (key, description) in hints {
            self.footer.add(&Text::new(&self.app, [key.bold(), format!(" {description}").dim()]));
        }
    }

    fn header_text(&self, model: &Model) -> Vec<Span<'static>> {
        let theme = self.app.theme();
        let mut spans = match model.connection {
            Connection::Connecting => vec!["Connecting".fg(theme.warning)],
            Connection::Connected => Vec::new(),
            Connection::Reconnecting => vec!["Reconnecting".fg(theme.warning)],
        };
        if let Some(settings) = model.settings() {
            if !spans.is_empty() {
                spans.push(Span::raw("  "));
            }
            let state = if settings.auto_switch { "on" } else { "off" };
            spans.push(format!("Auto switch {state} at {}%", settings.threshold).dim());
        }
        spans
    }

    fn status_text(&self, model: &Model) -> Vec<Span<'static>> {
        let theme = self.app.theme();
        if let Overlay::Remove { label, .. } = &model.overlay {
            return vec![
                format!("Remove {}? Press y to confirm.", truncate(label, PROMPT_LABEL_MAX)).fg(theme.warning),
            ];
        }
        if let Overlay::Reset { label, grants, index, confirming: true, .. } = &model.overlay {
            let chosen = &grants[*index];
            let detail = chosen.detail.as_deref().unwrap_or("Refills your usage limit now. This cannot be undone.");
            return vec![
                format!("Use {} on {}? Press y to confirm. {detail}", chosen.label, truncate(label, PROMPT_LABEL_MAX))
                    .fg(theme.warning),
            ];
        }
        if let Some(progress) = &model.busy {
            return vec![format!("{progress}...").into()];
        }
        if let Some(error) = &model.error {
            return vec![error.clone().fg(theme.error)];
        }
        if let Some(notice) = &model.notice {
            return vec![notice.clone().fg(theme.accent)];
        }
        model.disconnect_reason.iter().map(|reason| reason.clone().dim()).collect()
    }

    fn content_width(&self) -> usize {
        let scrollbar_and_margin = 2;
        usize::from(self.app.size().0).saturating_sub(scrollbar_and_margin)
    }

    fn reveal_selection(&self) {
        let model = self.model.borrow();
        if model.overlay == Overlay::Help || model.selected.is_none() {
            return;
        }
        let row_targets = self.row_targets.borrow();
        let is_selected = |target: &Option<Target>| *target == model.selected;
        let (Some(first), Some(last)) =
            (row_targets.iter().position(is_selected), row_targets.iter().rposition(is_selected))
        else {
            return;
        };
        let offset = self.list.scroll_position().1 as usize;
        let height = self.viewport.get().1 as usize;
        self.list.scroll_to(0.0, revealing_offset(offset, height, first, last + 1) as f64);
    }
}

fn revealing_offset(offset: usize, height: usize, start: usize, end: usize) -> usize {
    if end <= height {
        0
    } else if start < offset || end - start > height {
        start
    } else {
        offset.max(end - height)
    }
}

fn join_lines(lines: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut joined = Vec::new();
    for (index, line) in lines.into_iter().enumerate() {
        if index > 0 {
            joined.push(Span::raw("\n"));
        }
        joined.extend(line);
    }
    joined
}

#[cfg(test)]
mod tests;
