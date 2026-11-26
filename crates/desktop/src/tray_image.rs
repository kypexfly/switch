use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{
    NSAttributedStringNSStringDrawing, NSBezierPath, NSBitmapFormat, NSBitmapImageRep, NSColor, NSDeviceRGBColorSpace,
    NSFont, NSFontAttributeName, NSFontWeightBold, NSFontWeightSemibold, NSForegroundColorAttributeName,
    NSGraphicsContext,
};
use objc2_foundation::{NSAttributedString, NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::icons::provider_mark;
use crate::status::TrayGroup;

const SCALE: f64 = 2.0;
const PADDING: f64 = 2.0;
const GLYPH: f64 = 16.0;
const GLYPH_GAP: f64 = 4.0;
const COLUMN_GAP: f64 = 7.0;
const GROUP_GAP: f64 = 11.0;
const INACTIVE_ALPHA: f64 = 0.45;
const LINE_SPACING: f64 = -2.0;

pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

type Lines = Vec<Retained<NSAttributedString>>;

fn text_width(lines: &[Retained<NSAttributedString>]) -> f64 {
    lines.iter().map(|line| line.size().width).fold(0.0, f64::max).ceil()
}

fn columns_width(columns: &[Lines]) -> f64 {
    if columns.is_empty() {
        return 0.0;
    }
    GLYPH_GAP + columns.iter().map(|lines| text_width(lines)).sum::<f64>() + COLUMN_GAP * (columns.len() - 1) as f64
}

pub fn render(groups: &[TrayGroup], thickness: f64) -> Bitmap {
    let texts: Vec<Vec<Lines>> = groups
        .iter()
        .map(|group| group.columns.iter().map(|column| lines(&column.values, column.active)).collect())
        .collect();
    let content: f64 = texts.iter().map(|columns| GLYPH + columns_width(columns)).sum::<f64>()
        + GROUP_GAP * groups.len().saturating_sub(1) as f64;
    let width = (content + 2.0 * PADDING).ceil();
    let (pixels_wide, pixels_high) = ((width * SCALE) as isize, (thickness * SCALE).round() as isize);
    let rep = bitmap(pixels_wide, pixels_high);
    rep.setSize(NSSize::new(width, thickness));
    let context = NSGraphicsContext::graphicsContextWithBitmapImageRep(&rep).expect("a bitmap graphics context");
    NSGraphicsContext::saveGraphicsState_class();
    NSGraphicsContext::setCurrentContext(Some(&context));
    NSColor::blackColor().set();
    let mut x = PADDING;
    for (group, columns) in groups.iter().zip(&texts) {
        let glyph = NSRect::new(NSPoint::new(x, (thickness - GLYPH) / 2.0), NSSize::new(GLYPH, GLYPH));
        draw_mark(group, glyph);
        x += GLYPH;
        for (index, lines) in columns.iter().enumerate() {
            x += if index == 0 { GLYPH_GAP } else { COLUMN_GAP } + text_width(lines);
            draw_lines(lines, x, thickness);
        }
        x += GROUP_GAP;
    }
    context.flushGraphics();
    NSGraphicsContext::restoreGraphicsState_class();
    read_alpha(&rep, pixels_wide as usize, pixels_high as usize)
}

fn lines(values: &[String], active: bool) -> Lines {
    let (size, weight) =
        if values.len() == 1 { (12.0, unsafe { NSFontWeightBold }) } else { (9.0, unsafe { NSFontWeightSemibold }) };
    let font = NSFont::monospacedDigitSystemFontOfSize_weight(size, weight);
    let color = NSColor::blackColor().colorWithAlphaComponent(if active { 1.0 } else { INACTIVE_ALPHA });
    let keys = unsafe { [NSFontAttributeName, NSForegroundColorAttributeName] };
    let values_for_keys: [&AnyObject; 2] = [&font, &color];
    let attributes = NSDictionary::from_slices(&keys, &values_for_keys);
    values
        .iter()
        .map(|value| unsafe {
            NSAttributedString::initWithString_attributes(
                NSAttributedString::alloc(),
                &NSString::from_str(value),
                Some(&attributes),
            )
        })
        .collect()
}

fn draw_lines(lines: &[Retained<NSAttributedString>], right: f64, thickness: f64) {
    let line_height = lines.iter().map(|line| line.size().height).fold(0.0, f64::max);
    let block = line_height * lines.len() as f64 + LINE_SPACING * lines.len().saturating_sub(1) as f64;
    let mut top = (thickness + block) / 2.0;
    for line in lines {
        let size = line.size();
        line.drawAtPoint(NSPoint::new(right - size.width, top - line_height));
        top -= line_height + LINE_SPACING;
    }
}

fn draw_switch_mark(frame: NSRect) {
    let at = |x: f64, y: f64, width: f64, height: f64| {
        NSRect::new(NSPoint::new(frame.origin.x + x, frame.origin.y + y), NSSize::new(width, height))
    };
    let track = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(at(0.75, 3.75, 14.5, 8.5), 4.25, 4.25);
    track.setLineWidth(1.5);
    track.stroke();
    NSBezierPath::bezierPathWithOvalInRect(at(9.0, 6.0, 4.0, 4.0)).fill();
}

fn draw_mark(group: &TrayGroup, frame: NSRect) {
    NSColor::blackColor().set();
    if group.provider.is_empty() {
        draw_switch_mark(frame);
        return;
    }
    let Some(path) = provider_mark(&group.provider).and_then(|mark| svg_path(mark.body).ok()) else {
        NSBezierPath::bezierPathWithOvalInRect(NSRect::new(
            NSPoint::new(frame.origin.x + 0.5, frame.origin.y + 0.5),
            NSSize::new(frame.size.width - 1.0, frame.size.height - 1.0),
        ))
        .fill();
        return;
    };
    let [min_x, min_y, max_x, max_y] = bounds(&path);
    let side = frame.size.width * (1.0 - 2.0 * 0.04);
    let scale = side / (max_x - min_x).max(max_y - min_y);
    let (offset_x, offset_y) = (
        frame.origin.x + (frame.size.width - (max_x - min_x) * scale) / 2.0,
        frame.origin.y + (frame.size.height - (max_y - min_y) * scale) / 2.0,
    );
    let point = |(x, y): (f64, f64)| NSPoint::new(offset_x + (x - min_x) * scale, offset_y + (max_y - y) * scale);
    let bezier = NSBezierPath::bezierPath();
    for segment in &path {
        match *segment {
            Segment::Move(p) => bezier.moveToPoint(point(p)),
            Segment::Line(p) => bezier.lineToPoint(point(p)),
            Segment::Cubic(c1, c2, p) => {
                bezier.curveToPoint_controlPoint1_controlPoint2(point(p), point(c1), point(c2))
            }
            Segment::Close => bezier.closePath(),
        }
    }
    bezier.fill();
}

fn bitmap(width: isize, height: isize) -> Retained<NSBitmapImageRep> {
    unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            std::ptr::null_mut(),
            width,
            height,
            8,
            4,
            true,
            false,
            NSDeviceRGBColorSpace,
            NSBitmapFormat::empty(),
            width * 4,
            32,
        )
    }
    .expect("a tray bitmap")
}

fn read_alpha(rep: &NSBitmapImageRep, width: usize, height: usize) -> Bitmap {
    let stride = rep.bytesPerRow() as usize;
    let data = unsafe { std::slice::from_raw_parts(rep.bitmapData(), stride * height) };
    let mut rgba = Vec::with_capacity(width * height * 4);
    for row in data.chunks_exact(stride) {
        for [_, _, _, alpha] in row[..width * 4].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[0, 0, 0, *alpha]);
        }
    }
    Bitmap { width: width as u32, height: height as u32, rgba }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Segment {
    Move((f64, f64)),
    Line((f64, f64)),
    Cubic((f64, f64), (f64, f64), (f64, f64)),
    Close,
}

fn bounds(path: &[Segment]) -> [f64; 4] {
    let points = path.iter().flat_map(|segment| match *segment {
        Segment::Move(p) | Segment::Line(p) => vec![p],
        Segment::Cubic(c1, c2, p) => vec![c1, c2, p],
        Segment::Close => vec![],
    });
    points.fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |[x0, y0, x1, y1], (x, y)| {
        [x0.min(x), y0.min(y), x1.max(x), y1.max(y)]
    })
}

fn svg_path(svg: &str) -> Result<Vec<Segment>, String> {
    let mut segments = Vec::new();
    for attribute in svg.split(" d=\"").skip(1) {
        let data = attribute.split('"').next().unwrap_or_default();
        segments.extend(parse_path(data)?);
    }
    if segments.is_empty() { Err("the SVG has no path".into()) } else { Ok(segments) }
}

fn parse_path(data: &str) -> Result<Vec<Segment>, String> {
    let mut tokens = Tokens { chars: data.chars().peekable() };
    let (mut segments, mut current, mut start, mut command) = (Vec::new(), (0.0, 0.0), (0.0, 0.0), None);
    while let Some(next) = tokens.command().or(command) {
        let relative = next.is_ascii_lowercase();
        let origin = if relative { current } else { (0.0, 0.0) };
        let point = |tokens: &mut Tokens| -> Result<(f64, f64), String> {
            Ok((origin.0 + tokens.number()?, origin.1 + tokens.number()?))
        };
        let segment = match next.to_ascii_uppercase() {
            'M' => {
                let p = point(&mut tokens)?;
                start = p;
                command = Some(if relative { 'l' } else { 'L' });
                Segment::Move(p)
            }
            'L' => Segment::Line(point(&mut tokens)?),
            'H' => Segment::Line((tokens.number()? + origin.0, current.1)),
            'V' => Segment::Line((current.0, tokens.number()? + origin.1)),
            'C' => Segment::Cubic(point(&mut tokens)?, point(&mut tokens)?, point(&mut tokens)?),
            'Z' => {
                command = None;
                current = start;
                segments.push(Segment::Close);
                continue;
            }
            other => return Err(format!("unsupported path command {other}")),
        };
        if !matches!(next, 'M' | 'm') {
            command = Some(next);
        }
        if let Segment::Move(p) | Segment::Line(p) | Segment::Cubic(_, _, p) = segment {
            current = p;
        }
        segments.push(segment);
        if !tokens.has_number() && tokens.peek_command().is_none() {
            break;
        }
    }
    Ok(segments)
}

struct Tokens<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Tokens<'_> {
    fn skip_separators(&mut self) {
        while self.chars.next_if(|c| c.is_whitespace() || *c == ',').is_some() {}
    }

    fn peek_command(&mut self) -> Option<char> {
        self.skip_separators();
        self.chars.peek().copied().filter(|c| c.is_ascii_alphabetic() && !matches!(c, 'e' | 'E'))
    }

    fn command(&mut self) -> Option<char> {
        let command = self.peek_command()?;
        self.chars.next();
        Some(command)
    }

    fn has_number(&mut self) -> bool {
        self.skip_separators();
        self.chars.peek().is_some_and(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.'))
    }

    fn number(&mut self) -> Result<f64, String> {
        self.skip_separators();
        let mut text = String::new();
        if let Some(sign) = self.chars.next_if(|c| matches!(c, '-' | '+')) {
            text.push(sign);
        }
        let mut seen_dot = false;
        while let Some(c) = self.chars.next_if(|c| c.is_ascii_digit() || (*c == '.' && !seen_dot)) {
            seen_dot |= c == '.';
            text.push(c);
        }
        if let Some(e) = self.chars.next_if(|c| matches!(c, 'e' | 'E')) {
            text.push(e);
            if let Some(sign) = self.chars.next_if(|c| matches!(c, '-' | '+')) {
                text.push(sign);
            }
            while let Some(c) = self.chars.next_if(char::is_ascii_digit) {
                text.push(c);
            }
        }
        text.parse().map_err(|_| format!("invalid number {text:?} in path"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::TrayColumn;

    #[test]
    fn paths_parse_absolute_relative_and_implicit_commands() {
        let path = parse_path("M1 2L3 4H5V6C7 8 9 10 11 12Z m1-1 2.5.5h1z").unwrap();
        assert_eq!(
            path[..5],
            [
                Segment::Move((1.0, 2.0)),
                Segment::Line((3.0, 4.0)),
                Segment::Line((5.0, 4.0)),
                Segment::Line((5.0, 6.0)),
                Segment::Cubic((7.0, 8.0), (9.0, 10.0), (11.0, 12.0)),
            ]
        );
        assert_eq!(
            path[6..],
            [Segment::Move((2.0, 1.0)), Segment::Line((4.5, 1.5)), Segment::Line((5.5, 1.5)), Segment::Close,]
        );
    }

    #[test]
    fn provider_marks_parse() {
        for provider in ["claude", "antigravity"] {
            let path = svg_path(provider_mark(provider).unwrap().body).unwrap();
            let [x0, y0, x1, y1] = bounds(&path);
            assert!(x0 > 0.0 && y0 > 0.0 && x1 < 100.0 && y1 < 100.0, "{provider} bounds: {x0}, {y0}, {x1}, {y1}");
        }
    }

    #[test]
    fn the_icon_has_the_bar_height_and_grows_with_text() {
        let logo = render(&[TrayGroup { provider: "claude".into(), columns: vec![] }], 24.0);
        assert_eq!((logo.width, logo.height), (40, 48));
        let column = |active| TrayColumn { values: vec!["97%".into(), "16%".into()], active };
        let two = render(&[TrayGroup { provider: "claude".into(), columns: vec![column(true)] }], 24.0);
        assert!(two.width > logo.width + 30);
        let both = render(&[TrayGroup { provider: "claude".into(), columns: vec![column(true), column(false)] }], 24.0);
        assert!(both.width > two.width + 30);
        assert_eq!(two.rgba.len(), (two.width * two.height * 4) as usize);
        let row_has_ink = |bitmap: &Bitmap, y: u32| {
            (0..bitmap.width).any(|x| bitmap.rgba[((y * bitmap.width + x) * 4 + 3) as usize] > 0)
        };
        assert!(
            !row_has_ink(&two, 0) && !row_has_ink(&two, two.height - 1),
            "keeps transparent padding at the top and bottom"
        );
    }

    #[test]
    fn provider_mark_after_an_inactive_account_stays_at_full_opacity() {
        let inactive = TrayColumn { values: vec!["14%".into()], active: false };
        let groups = [
            TrayGroup { provider: "claude".into(), columns: vec![inactive] },
            TrayGroup { provider: "antigravity".into(), columns: vec![] },
        ];
        let bitmap = render(&groups, 24.0);
        let mark_start_x = (bitmap.width - (GLYPH + PADDING).ceil() as u32 * 2) as usize;
        let max_alpha = (0..bitmap.height as usize)
            .flat_map(|y| (mark_start_x..bitmap.width as usize).map(move |x| (x, y)))
            .map(|(x, y)| bitmap.rgba[(y * bitmap.width as usize + x) * 4 + 3])
            .max()
            .unwrap_or(0);
        assert!(max_alpha >= 250, "expected full-opacity mark after inactive column, got max alpha {max_alpha}");
    }
}
