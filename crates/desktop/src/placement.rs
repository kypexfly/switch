#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Screen {
    pub frame: Rect,
    pub visible: Rect,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelFrame {
    pub x: f64,
    pub y: f64,
    pub max_height: f64,
}

const GAP: f64 = 4.0;
const MARGIN: f64 = 8.0;

pub fn panel_frame(tray_pixels: [i32; 4], screens: &[Screen], bar_thickness: f64, width: f64) -> Option<PanelFrame> {
    let candidates: Vec<(&Screen, Rect)> = screens
        .iter()
        .filter_map(|screen| {
            let [x, y, w, h] = tray_pixels.map(|value| f64::from(value) / screen.scale);
            screen.frame.contains(x + w / 2.0, y + h / 2.0).then_some((screen, Rect { x, y, width: w, height: h }))
        })
        .collect();
    let (screen, tray) = candidates
        .iter()
        .find(|(_, tray)| (tray.height - bar_thickness).abs() < 2.0)
        .or(candidates.first())
        .copied()?;
    let area = screen.visible;
    let centered = tray.x + tray.width / 2.0 - width / 2.0;
    let x = centered.min(area.x + area.width - width - MARGIN).max(area.x + MARGIN);
    let y = (tray.y + tray.height).max(area.y) + GAP;
    Some(PanelFrame { x: x.round(), y: y.round(), max_height: (area.y + area.height - y - MARGIN).max(120.0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(x: f64, width: f64, height: f64, scale: f64) -> Screen {
        Screen {
            frame: Rect { x, y: 0.0, width, height },
            visible: Rect { x, y: 24.0, width, height: height - 24.0 - 70.0 },
            scale,
        }
    }

    #[test]
    fn opens_centered_under_the_item() {
        let frame = panel_frame([2000, 0, 120, 48], &[screen(0.0, 1512.0, 982.0, 2.0)], 24.0, 340.0).unwrap();
        assert_eq!(frame, PanelFrame { x: 860.0, y: 28.0, max_height: 982.0 - 70.0 - 28.0 - 8.0 });
    }

    #[test]
    fn stays_inside_the_right_edge() {
        let frame = panel_frame([2960, 0, 60, 48], &[screen(0.0, 1512.0, 982.0, 2.0)], 24.0, 340.0).unwrap();
        assert_eq!(frame.x, 1512.0 - 340.0 - 8.0);
    }

    #[test]
    fn follows_the_item_onto_a_secondary_display_with_its_own_scale() {
        let screens = [screen(0.0, 1512.0, 982.0, 2.0), screen(1512.0, 1920.0, 1080.0, 1.0)];
        let frame = panel_frame([1600, 0, 40, 24], &screens, 24.0, 340.0).unwrap();
        assert_eq!(frame.x, 1520.0);
        let left = [screen(-1920.0, 1920.0, 1080.0, 1.0), screen(0.0, 1512.0, 982.0, 2.0)];
        assert_eq!(panel_frame([-30, 0, 30, 24], &left, 24.0, 340.0).unwrap().x, -340.0 - 8.0);
    }

    #[test]
    fn needs_a_screen_holding_the_item() {
        assert_eq!(panel_frame([9000, 9000, 10, 10], &[screen(0.0, 1512.0, 982.0, 2.0)], 24.0, 340.0), None);
    }
}
