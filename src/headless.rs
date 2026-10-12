//! Drawing a tab to a PNG without a window: subscribe to it, wait for its screen, draw it with
//! the same renderer the windows use.

use crate::client::{Client, Event};
use crate::model::Screen;
use crate::render::{self, Damage, FontCache, Surface, TileFrame};
use ctx_protocol::*;
use std::time::{Duration, Instant};

pub fn render_main(args: &[String]) -> i32 {
    let (Some(tab), Some(out)) = (args.first().and_then(|t| t.parse::<u64>().ok()), args.get(1)) else {
        eprintln!("usage: CTX --render <tab> <file.png> [columns rows] [theme]");
        return 2;
    };
    let columns: u16 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    let rows: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);
    let theme_name = args.get(4).cloned().unwrap_or_else(|| "basic".into());
    match render(tab, std::path::Path::new(out), columns, rows, &theme_name) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

pub fn render(tab: u64, out: &std::path::Path, columns: u16, rows: u16, theme_name: &str) -> Result<(), String> {
    let (mut client, events) = Client::connect(|| {}).map_err(|e| format!("{e:?}"))?;
    let theme = crate::theme::load(theme_name, std::path::Path::new("/nonexistent"), false)?;
    client.tell(Request::SetLook { tab, look: theme.look(CursorShape::Block, false) });
    if columns > 0 && rows > 0 {
        client.input(tab, AppInput::Resize { columns, rows });
    }
    client.tell(Request::Subscribe { tabs: vec![tab] });
    let mut screen = Screen::new(tab);
    let deadline = Instant::now() + Duration::from_secs(5);
    // Take updates until the screen has been quiet for a moment.
    let mut last = Instant::now();
    while Instant::now() < deadline {
        match events.recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Stream(StreamOut::Screen(u))) if u.tab == tab => {
                screen.apply(u);
                last = Instant::now();
            }
            Ok(Event::Disconnected(m)) => return Err(m),
            Ok(_) => {}
            Err(_) => {
                if screen.received && last.elapsed() > Duration::from_millis(300) {
                    break;
                }
            }
        }
    }
    if !screen.received {
        return Err(format!("No screen arrived for tab {tab}."));
    }
    let mut fonts = FontCache::default();
    let f = fonts.get("SF Mono", 13.0, 2.0);
    let w = (screen.columns as f64 * f.cell_w + 2.0 * render::PAD_X * 2.0).ceil();
    let h = (screen.rows as f64 * f.cell_h + 2.0 * render::PAD_Y * 2.0).ceil();
    let mut surface = Surface::new_heap(w as usize, h as usize);
    let tile = TileFrame {
        x: 0.0,
        y: 0.0,
        w,
        h,
        fonts: f,
        rows: screen.lines.iter().map(|r| (r, false)).collect(),
        foreground: screen.foreground,
        background: screen.background,
        selection_color: theme.selection,
        cursor: screen.cursor.filter(|_| screen.cursor_visible).map(|(x, y)| render::CursorFrame {
            column: x as usize,
            row: y as usize,
            shape: screen.cursor_shape,
            focused: true,
            color: theme.cursor,
        }),
        selection: Vec::new(),
        matches: Vec::new(),
        link: None,
        marked: None,
        scrolled: None,
    };
    render::draw(&mut surface, &[tile], &Damage::All, theme.background);
    surface.write_png(out)
}

/// Draw the app icon at the sizes an `.iconset` needs (used by `cargo xtask bundle`).
pub fn icon_main(dir: &std::path::Path) -> i32 {
    let _ = std::fs::create_dir_all(dir);
    for (name, px) in [
        ("icon_16x16", 16),
        ("icon_16x16@2x", 32),
        ("icon_32x32", 32),
        ("icon_32x32@2x", 64),
        ("icon_128x128", 128),
        ("icon_128x128@2x", 256),
        ("icon_256x256", 256),
        ("icon_256x256@2x", 512),
        ("icon_512x512", 512),
        ("icon_512x512@2x", 1024),
    ] {
        if let Err(e) = icon(px, &dir.join(format!("{name}.png"))) {
            eprintln!("{e}");
            return 1;
        }
    }
    0
}

fn icon(px: usize, path: &std::path::Path) -> Result<(), String> {
    use crate::text::Row;
    use ctx_protocol::{CellStyle, StyledRun};
    let mut s = Surface::new_heap(px, px);
    let scale = px as f64 / 64.0;
    let mut fonts = FontCache::default();
    let f = fonts.get("SF Mono", 22.0, scale);
    let green = CellStyle { foreground: Some([80, 220, 120]), flags: 1, ..Default::default() };
    let row = Row { runs: vec![StyledRun { text: ">_".into(), style: green }], wraps: false, event: false };
    let margin = (6.0 * scale).round();
    let tile = TileFrame {
        x: margin,
        y: margin,
        w: px as f64 - 2.0 * margin,
        h: px as f64 - 2.0 * margin,
        fonts: f.clone(),
        rows: vec![(&row, false)],
        foreground: [240, 240, 240],
        background: [32, 34, 40],
        selection_color: [0, 0, 0],
        cursor: None,
        selection: Vec::new(),
        matches: Vec::new(),
        link: None,
        marked: None,
        scrolled: None,
    };
    // Transparent corners: start clear, then the dark tile.
    render::draw(&mut s, &[tile], &Damage::All, [0, 0, 0]);
    s.clear_outside_rounded(margin, (12.0 * scale).round());
    s.write_png(path)
}

/// Set a tab's size the way a window would (before running a screen test in it).
pub fn resize_tab(tab: u64, columns: u16, rows: u16) -> Result<(), String> {
    let (mut client, _events) = Client::connect(|| {}).map_err(|e| format!("{e:?}"))?;
    client
        .ask("", Request::Input { tab, input: AppInput::Resize { columns, rows } })
        .map(|_| ())
        .ok_or("no answer".into())
}
