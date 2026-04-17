//! Minecraft tooltip-style PNG renderer for Seller messages.
//!
//! Each selected item becomes one 420×440 PNG card roughly mimicking the
//! in-game tooltip look: dark translucent background, coloured title, greyed
//! lore lines, and the SkyBlock item icon placed prominently at the top.
//!
//! The font is the same 5×7 bitmap used by `og_image.rs` — scaled up at draw
//! time for legibility.  Keeping the renderer self-contained (no external
//! font files, no additional dependencies) matches the rest of this crate's
//! philosophy of shipping a single static binary.

use image::{imageops, ImageBuffer, Rgba, RgbaImage};
use std::io::Cursor;

// ── Colour palette ────────────────────────────────────────────────────────

const BG: Rgba<u8> = Rgba([16, 0, 16, 245]);
const BORDER_OUTER: Rgba<u8> = Rgba([80, 0, 255, 255]);
const BORDER_INNER: Rgba<u8> = Rgba([40, 0, 110, 255]);
const TITLE_DEFAULT: Rgba<u8> = Rgba([255, 255, 85, 255]); // yellow (MC §e)
const LORE_DEFAULT: Rgba<u8> = Rgba([170, 170, 170, 255]); // gray (MC §7)
const ICON_BG: Rgba<u8> = Rgba([30, 12, 40, 255]);
const COUNT_BG: Rgba<u8> = Rgba([0, 0, 0, 200]);
const COUNT_FG: Rgba<u8> = Rgba([255, 255, 255, 255]);

// ── Minecraft §-code → RGB ────────────────────────────────────────────────

fn mc_color(ch: char) -> Option<Rgba<u8>> {
    // Values from https://minecraft.wiki/w/Formatting_codes — we use the
    // "chat" variants since that's what lore strings are formatted with.
    let rgb = match ch {
        '0' => (0, 0, 0),
        '1' => (0, 0, 170),
        '2' => (0, 170, 0),
        '3' => (0, 170, 170),
        '4' => (170, 0, 0),
        '5' => (170, 0, 170),
        '6' => (255, 170, 0),
        '7' => (170, 170, 170),
        '8' => (85, 85, 85),
        '9' => (85, 85, 255),
        'a' => (85, 255, 85),
        'b' => (85, 255, 255),
        'c' => (255, 85, 85),
        'd' => (255, 85, 255),
        'e' => (255, 255, 85),
        'f' => (255, 255, 255),
        _ => return None,
    };
    Some(Rgba([rgb.0, rgb.1, rgb.2, 255]))
}

/// A single "run" of identically-coloured characters produced by parsing
/// a Minecraft formatted string.
struct ColoredRun {
    text: String,
    color: Rgba<u8>,
}

fn parse_mc_string(s: &str, default_color: Rgba<u8>) -> Vec<ColoredRun> {
    let mut runs: Vec<ColoredRun> = Vec::new();
    let mut current_color = default_color;
    let mut buffer = String::new();
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '§' {
            if let Some(&code) = chars.peek() {
                chars.next();
                // Colour code — flush current buffer and switch palette.
                // Formatting codes (k,l,m,n,o) are ignored for simplicity;
                // `r` resets to default.
                if let Some(col) = mc_color(code.to_ascii_lowercase()) {
                    if !buffer.is_empty() {
                        runs.push(ColoredRun {
                            text: std::mem::take(&mut buffer),
                            color: current_color,
                        });
                    }
                    current_color = col;
                } else if code == 'r' || code == 'R' {
                    if !buffer.is_empty() {
                        runs.push(ColoredRun {
                            text: std::mem::take(&mut buffer),
                            color: current_color,
                        });
                    }
                    current_color = default_color;
                }
                continue;
            }
        }
        buffer.push(ch);
    }
    if !buffer.is_empty() {
        runs.push(ColoredRun {
            text: buffer,
            color: current_color,
        });
    }
    runs
}

/// Strip Minecraft formatting codes entirely, returning plain text.
pub fn strip_mc_codes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '§' {
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

// ── Embedded 5×7 bitmap font (same layout as og_image) ────────────────────

const FONT: &[(char, [u8; 5])] = &[
    ('A', [0x7E, 0x11, 0x11, 0x11, 0x7E]),
    ('B', [0x7F, 0x49, 0x49, 0x49, 0x36]),
    ('C', [0x3E, 0x41, 0x41, 0x41, 0x22]),
    ('D', [0x7F, 0x41, 0x41, 0x41, 0x3E]),
    ('E', [0x7F, 0x49, 0x49, 0x49, 0x41]),
    ('F', [0x7F, 0x09, 0x09, 0x09, 0x01]),
    ('G', [0x3E, 0x41, 0x49, 0x49, 0x7A]),
    ('H', [0x7F, 0x08, 0x08, 0x08, 0x7F]),
    ('I', [0x00, 0x41, 0x7F, 0x41, 0x00]),
    ('J', [0x20, 0x40, 0x41, 0x3F, 0x01]),
    ('K', [0x7F, 0x08, 0x14, 0x22, 0x41]),
    ('L', [0x7F, 0x40, 0x40, 0x40, 0x40]),
    ('M', [0x7F, 0x02, 0x0C, 0x02, 0x7F]),
    ('N', [0x7F, 0x04, 0x08, 0x10, 0x7F]),
    ('O', [0x3E, 0x41, 0x41, 0x41, 0x3E]),
    ('P', [0x7F, 0x09, 0x09, 0x09, 0x06]),
    ('Q', [0x3E, 0x41, 0x51, 0x21, 0x5E]),
    ('R', [0x7F, 0x09, 0x19, 0x29, 0x46]),
    ('S', [0x46, 0x49, 0x49, 0x49, 0x31]),
    ('T', [0x01, 0x01, 0x7F, 0x01, 0x01]),
    ('U', [0x3F, 0x40, 0x40, 0x40, 0x3F]),
    ('V', [0x1F, 0x20, 0x40, 0x20, 0x1F]),
    ('W', [0x3F, 0x40, 0x38, 0x40, 0x3F]),
    ('X', [0x63, 0x14, 0x08, 0x14, 0x63]),
    ('Y', [0x07, 0x08, 0x70, 0x08, 0x07]),
    ('Z', [0x61, 0x51, 0x49, 0x45, 0x43]),
    ('a', [0x20, 0x54, 0x54, 0x54, 0x78]),
    ('b', [0x7F, 0x48, 0x44, 0x44, 0x38]),
    ('c', [0x38, 0x44, 0x44, 0x44, 0x20]),
    ('d', [0x38, 0x44, 0x44, 0x48, 0x7F]),
    ('e', [0x38, 0x54, 0x54, 0x54, 0x18]),
    ('f', [0x08, 0x7E, 0x09, 0x01, 0x02]),
    ('g', [0x0C, 0x52, 0x52, 0x52, 0x3E]),
    ('h', [0x7F, 0x08, 0x04, 0x04, 0x78]),
    ('i', [0x00, 0x44, 0x7D, 0x40, 0x00]),
    ('j', [0x20, 0x40, 0x44, 0x3D, 0x00]),
    ('k', [0x7F, 0x10, 0x28, 0x44, 0x00]),
    ('l', [0x00, 0x41, 0x7F, 0x40, 0x00]),
    ('m', [0x7C, 0x04, 0x18, 0x04, 0x78]),
    ('n', [0x7C, 0x08, 0x04, 0x04, 0x78]),
    ('o', [0x38, 0x44, 0x44, 0x44, 0x38]),
    ('p', [0x7C, 0x14, 0x14, 0x14, 0x08]),
    ('q', [0x08, 0x14, 0x14, 0x18, 0x7C]),
    ('r', [0x7C, 0x08, 0x04, 0x04, 0x08]),
    ('s', [0x48, 0x54, 0x54, 0x54, 0x20]),
    ('t', [0x04, 0x3F, 0x44, 0x40, 0x20]),
    ('u', [0x3C, 0x40, 0x40, 0x20, 0x7C]),
    ('v', [0x1C, 0x20, 0x40, 0x20, 0x1C]),
    ('w', [0x3C, 0x40, 0x30, 0x40, 0x3C]),
    ('x', [0x44, 0x28, 0x10, 0x28, 0x44]),
    ('y', [0x0C, 0x50, 0x50, 0x50, 0x3C]),
    ('z', [0x44, 0x64, 0x54, 0x4C, 0x44]),
    ('0', [0x3E, 0x51, 0x49, 0x45, 0x3E]),
    ('1', [0x00, 0x42, 0x7F, 0x40, 0x00]),
    ('2', [0x42, 0x61, 0x51, 0x49, 0x46]),
    ('3', [0x21, 0x41, 0x45, 0x4B, 0x31]),
    ('4', [0x18, 0x14, 0x12, 0x7F, 0x10]),
    ('5', [0x27, 0x45, 0x45, 0x45, 0x39]),
    ('6', [0x3C, 0x4A, 0x49, 0x49, 0x30]),
    ('7', [0x01, 0x71, 0x09, 0x05, 0x03]),
    ('8', [0x36, 0x49, 0x49, 0x49, 0x36]),
    ('9', [0x06, 0x49, 0x49, 0x29, 0x1E]),
    (' ', [0x00, 0x00, 0x00, 0x00, 0x00]),
    ('.', [0x00, 0x60, 0x60, 0x00, 0x00]),
    (',', [0x00, 0x80, 0x60, 0x00, 0x00]),
    (':', [0x00, 0x36, 0x36, 0x00, 0x00]),
    (';', [0x00, 0xAC, 0x6C, 0x00, 0x00]),
    ('/', [0x20, 0x10, 0x08, 0x04, 0x02]),
    ('\\', [0x02, 0x04, 0x08, 0x10, 0x20]),
    ('-', [0x08, 0x08, 0x08, 0x08, 0x08]),
    ('+', [0x08, 0x08, 0x3E, 0x08, 0x08]),
    ('=', [0x14, 0x14, 0x14, 0x14, 0x14]),
    ('(', [0x00, 0x1C, 0x22, 0x41, 0x00]),
    (')', [0x00, 0x41, 0x22, 0x1C, 0x00]),
    ('[', [0x00, 0x7F, 0x41, 0x41, 0x00]),
    (']', [0x00, 0x41, 0x41, 0x7F, 0x00]),
    ('|', [0x00, 0x00, 0x7F, 0x00, 0x00]),
    ('%', [0x23, 0x13, 0x08, 0x64, 0x62]),
    ('_', [0x40, 0x40, 0x40, 0x40, 0x40]),
    ('!', [0x00, 0x00, 0x5F, 0x00, 0x00]),
    ('?', [0x02, 0x01, 0x51, 0x09, 0x06]),
    ('*', [0x14, 0x08, 0x3E, 0x08, 0x14]),
    ('#', [0x14, 0x7F, 0x14, 0x7F, 0x14]),
    ('\'', [0x00, 0x05, 0x07, 0x00, 0x00]),
    ('"', [0x00, 0x07, 0x00, 0x07, 0x00]),
    ('&', [0x36, 0x49, 0x55, 0x22, 0x50]),
    ('<', [0x08, 0x14, 0x22, 0x41, 0x00]),
    ('>', [0x00, 0x41, 0x22, 0x14, 0x08]),
];

fn glyph(ch: char) -> [u8; 5] {
    for &(c, bits) in FONT {
        if c == ch {
            return bits;
        }
    }
    [0x7E, 0x7E, 0x7E, 0x7E, 0x7E]
}

fn text_width(s: &str, scale: u32) -> u32 {
    let chars = s.chars().count() as u32;
    if chars == 0 {
        return 0;
    }
    (chars * (5 * scale + scale)) - scale
}

fn draw_char(img: &mut RgbaImage, ch: char, x: u32, y: u32, scale: u32, color: Rgba<u8>) {
    let bits = glyph(ch);
    for col in 0..5u32 {
        let col_bits = bits[col as usize];
        for row in 0..7u32 {
            if (col_bits >> row) & 1 == 1 {
                let px = x + col * scale;
                let py = y + row * scale;
                for dy in 0..scale {
                    for dx in 0..scale {
                        let fx = px + dx;
                        let fy = py + dy;
                        if fx < img.width() && fy < img.height() {
                            img.put_pixel(fx, fy, color);
                        }
                    }
                }
            }
        }
    }
}

fn draw_text(img: &mut RgbaImage, text: &str, x: u32, y: u32, scale: u32, color: Rgba<u8>) {
    let mut cx = x;
    let advance = 5 * scale + scale;
    for ch in text.chars() {
        draw_char(img, ch, cx, y, scale, color);
        cx += advance;
    }
}

fn draw_colored_text(
    img: &mut RgbaImage,
    runs: &[ColoredRun],
    x: u32,
    y: u32,
    scale: u32,
) {
    let advance = 5 * scale + scale;
    let mut cx = x;
    for run in runs {
        for ch in run.text.chars() {
            draw_char(img, ch, cx, y, scale, run.color);
            cx += advance;
        }
    }
}

fn fill_rect(img: &mut RgbaImage, x: u32, y: u32, w: u32, h: u32, color: Rgba<u8>) {
    for py in y..y.saturating_add(h).min(img.height()) {
        for px in x..x.saturating_add(w).min(img.width()) {
            img.put_pixel(px, py, color);
        }
    }
}

fn draw_rect_outline(
    img: &mut RgbaImage,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    thickness: u32,
    color: Rgba<u8>,
) {
    fill_rect(img, x, y, w, thickness, color);
    fill_rect(img, x, y + h.saturating_sub(thickness), w, thickness, color);
    fill_rect(img, x, y, thickness, h, color);
    fill_rect(img, x + w.saturating_sub(thickness), y, thickness, h, color);
}

/// Downsample long strings so they fit within `max_width` pixels at the given
/// scale, appending an ellipsis when truncation occurs.
fn truncate_to_width(s: &str, max_width: u32, scale: u32) -> String {
    if text_width(s, scale) <= max_width {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while !chars.is_empty()
        && text_width(
            &format!("{}{}", chars.iter().collect::<String>(), "..."),
            scale,
        ) > max_width
    {
        chars.pop();
    }
    format!("{}...", chars.iter().collect::<String>())
}

// ── Item description DTO ──────────────────────────────────────────────────

/// Everything needed to render an item card.  Lore strings may contain
/// Minecraft colour codes (`§x`) — they're parsed at draw time.
#[derive(Clone, Debug)]
pub struct RenderableItem {
    pub title: String,
    pub lore: Vec<String>,
    pub count: u32,
    pub icon_png: Option<Vec<u8>>,
    /// Optional extra line drawn at the bottom of the card in accent colour
    /// (e.g. the asking price or auction end time).
    pub footer: Option<String>,
}

// ── Public renderer ───────────────────────────────────────────────────────

const CARD_W: u32 = 420;
const CARD_H: u32 = 440;
const PADDING: u32 = 18;
const ICON_SIZE: u32 = 96;

const TITLE_SCALE: u32 = 3; // 15×21 px per glyph
const LORE_SCALE: u32 = 2; // 10×14 px per glyph
const FOOTER_SCALE: u32 = 2;

/// Produce a PNG `Vec<u8>` for the given item.  Never fails: if something
/// goes wrong during rendering (e.g. a decode error on the icon) we fall back
/// to a plain card with a placeholder icon.
pub fn render_item_png(item: &RenderableItem) -> Vec<u8> {
    let mut img: RgbaImage = ImageBuffer::from_pixel(CARD_W, CARD_H, BG);

    // Double border (outer thin + inner thicker) to echo the MC tooltip.
    draw_rect_outline(&mut img, 0, 0, CARD_W, CARD_H, 2, BORDER_OUTER);
    draw_rect_outline(&mut img, 3, 3, CARD_W - 6, CARD_H - 6, 1, BORDER_INNER);

    // Title — colour-aware, truncated to fit.
    let title_runs = parse_mc_string(&item.title, TITLE_DEFAULT);
    let title_plain = title_runs.iter().map(|r| r.text.clone()).collect::<String>();
    let title_fit = truncate_to_width(&title_plain, CARD_W - 2 * PADDING, TITLE_SCALE);
    // Re-derive runs for the truncated string by mapping characters back to
    // their original colour positions.
    let title_runs_fit = reapply_colors(&title_runs, &title_fit);
    let title_w = text_width(&title_fit, TITLE_SCALE);
    let title_x = (CARD_W - title_w) / 2;
    draw_colored_text(&mut img, &title_runs_fit, title_x, PADDING + 4, TITLE_SCALE);

    // Icon block.
    let icon_x = (CARD_W - ICON_SIZE) / 2;
    let icon_y = PADDING + 40;
    fill_rect(&mut img, icon_x - 4, icon_y - 4, ICON_SIZE + 8, ICON_SIZE + 8, ICON_BG);
    draw_rect_outline(
        &mut img,
        icon_x - 4,
        icon_y - 4,
        ICON_SIZE + 8,
        ICON_SIZE + 8,
        1,
        BORDER_INNER,
    );

    if let Some(bytes) = &item.icon_png {
        if let Ok(decoded) = image::load_from_memory(bytes) {
            let resized = imageops::resize(
                &decoded,
                ICON_SIZE,
                ICON_SIZE,
                imageops::FilterType::Nearest,
            );
            for (dx, dy, px) in resized.enumerate_pixels() {
                let fx = icon_x + dx;
                let fy = icon_y + dy;
                if fx < img.width() && fy < img.height() && px.0[3] > 0 {
                    // Alpha-composite against the icon background for nicer edges.
                    blend_pixel(&mut img, fx, fy, *px);
                }
            }
        } else {
            draw_missing_icon(&mut img, icon_x, icon_y);
        }
    } else {
        draw_missing_icon(&mut img, icon_x, icon_y);
    }

    // Stack count badge (only shown for counts > 1).
    if item.count > 1 {
        let count_str = item.count.to_string();
        let cw = text_width(&count_str, LORE_SCALE) + 8;
        let ch = 14u32 * LORE_SCALE / 2 + 6;
        let cx = icon_x + ICON_SIZE - cw;
        let cy = icon_y + ICON_SIZE - ch;
        fill_rect(&mut img, cx, cy, cw, ch, COUNT_BG);
        draw_text(&mut img, &count_str, cx + 4, cy + 3, LORE_SCALE, COUNT_FG);
    }

    // Lore lines.
    let lore_start_y = icon_y + ICON_SIZE + 18;
    let line_height = 7 * LORE_SCALE + LORE_SCALE; // glyph height + gap
    let mut cy = lore_start_y;
    let max_w = CARD_W - 2 * PADDING;
    let footer_reserved = if item.footer.is_some() {
        line_height + 8
    } else {
        0
    };
    for raw in &item.lore {
        if cy + line_height + footer_reserved >= CARD_H - PADDING {
            break;
        }
        let runs = parse_mc_string(raw, LORE_DEFAULT);
        let plain: String = runs.iter().map(|r| r.text.clone()).collect();
        let fit = truncate_to_width(&plain, max_w, LORE_SCALE);
        let runs_fit = reapply_colors(&runs, &fit);
        draw_colored_text(&mut img, &runs_fit, PADDING, cy, LORE_SCALE);
        cy += line_height;
    }

    // Footer line at the bottom (price / auction info).
    if let Some(footer) = &item.footer {
        let runs = parse_mc_string(footer, Rgba([255, 170, 0, 255]));
        let plain: String = runs.iter().map(|r| r.text.clone()).collect();
        let fit = truncate_to_width(&plain, max_w, FOOTER_SCALE);
        let runs_fit = reapply_colors(&runs, &fit);
        let fw = text_width(&fit, FOOTER_SCALE);
        let fx = (CARD_W - fw) / 2;
        let fy = CARD_H - PADDING - (7 * FOOTER_SCALE);
        draw_colored_text(&mut img, &runs_fit, fx, fy, FOOTER_SCALE);
    }

    encode_png(&img)
}

fn draw_missing_icon(img: &mut RgbaImage, x: u32, y: u32) {
    fill_rect(img, x, y, ICON_SIZE, ICON_SIZE, ICON_BG);
    let q = "?";
    let w = text_width(q, 8);
    let h = 7 * 8;
    let qx = x + (ICON_SIZE - w) / 2;
    let qy = y + (ICON_SIZE - h) / 2;
    draw_text(img, q, qx, qy, 8, Rgba([120, 120, 160, 255]));
}

fn blend_pixel(img: &mut RgbaImage, x: u32, y: u32, px: Rgba<u8>) {
    if px.0[3] == 255 {
        img.put_pixel(x, y, px);
        return;
    }
    let dst = img.get_pixel(x, y).0;
    let sa = px.0[3] as f32 / 255.0;
    let out = [
        (px.0[0] as f32 * sa + dst[0] as f32 * (1.0 - sa)) as u8,
        (px.0[1] as f32 * sa + dst[1] as f32 * (1.0 - sa)) as u8,
        (px.0[2] as f32 * sa + dst[2] as f32 * (1.0 - sa)) as u8,
        255,
    ];
    img.put_pixel(x, y, Rgba(out));
}

fn encode_png(img: &RgbaImage) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(32 * 1024);
    let mut cursor = Cursor::new(&mut buf);
    // `image::codecs::png::PngEncoder` requires the dimensions; using
    // `DynamicImage::write_to` keeps things simple.
    let (w, h) = img.dimensions();
    let dyn_img = image::DynamicImage::ImageRgba8(img.clone());
    let _ = dyn_img.write_to(&mut cursor, image::ImageFormat::Png);
    let _ = (w, h);
    buf
}

/// Re-apply colouring from a sequence of runs to a (possibly shorter/longer)
/// string.  We do this instead of re-parsing because the truncation helper
/// operates on the plain text only.
fn reapply_colors(runs: &[ColoredRun], target: &str) -> Vec<ColoredRun> {
    let mut out: Vec<ColoredRun> = Vec::new();
    let mut remaining = target.to_string();
    for run in runs {
        if remaining.is_empty() {
            break;
        }
        let take = run.text.chars().count().min(remaining.chars().count());
        if take == 0 {
            continue;
        }
        let chunk: String = remaining.chars().take(take).collect();
        remaining = remaining.chars().skip(take).collect();
        out.push(ColoredRun {
            text: chunk,
            color: run.color,
        });
    }
    // If truncation appended a trailing "..." it won't be covered by any run
    // — draw it in the last-used colour to match the rest of the line.
    if !remaining.is_empty() {
        let last_color = runs
            .last()
            .map(|r| r.color)
            .unwrap_or(LORE_DEFAULT);
        out.push(ColoredRun {
            text: remaining,
            color: last_color,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_mc_codes("§6Gold §7Gray"), "Gold Gray");
        assert_eq!(strip_mc_codes("no codes"), "no codes");
    }

    #[test]
    fn renders_minimal_card_without_panicking() {
        let item = RenderableItem {
            title: "§6Hyperion".to_string(),
            lore: vec![
                "§7Damage: §c+260".to_string(),
                "§7A legendary weapon.".to_string(),
            ],
            count: 1,
            icon_png: None,
            footer: Some("§ePrice: 1,000,000,000".to_string()),
        };
        let png = render_item_png(&item);
        assert!(png.len() > 100, "png should have meaningful body");
        assert_eq!(&png[0..8], &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']);
    }
}
