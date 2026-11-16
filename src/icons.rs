//! Toolbar icons and the icon-above-label toolbar button.
//!
//! PNG icons come from oddity-picview's set (Haiku OS icons, MIT, see assets/icons).
//! Drive, stop, shield and the two view icons are drawn in code.

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontId, Painter, Rect, Response, Sense, Shape,
    Stroke, StrokeKind, TextureHandle, Ui,
};

pub struct Icons {
    pub folder: TextureHandle,
    pub scan: TextureHandle,
    pub settings: TextureHandle,
}

fn load(ctx: &egui::Context, name: &str, png: &[u8]) -> TextureHandle {
    let img = image::load_from_memory(png).expect("embedded icon").to_rgba8();
    let ci = egui::ColorImage::from_rgba_unmultiplied(
        [img.width() as usize, img.height() as usize],
        img.as_raw(),
    );
    ctx.load_texture(
        format!("icon/{name}"),
        ci,
        egui::TextureOptions {
            mipmap_mode: Some(egui::TextureFilter::Linear),
            ..egui::TextureOptions::LINEAR
        },
    )
}

impl Icons {
    pub fn new(ctx: &egui::Context) -> Self {
        Icons {
            folder: load(ctx, "folder", include_bytes!("../assets/icons/folder.png")),
            scan: load(ctx, "scan", include_bytes!("../assets/icons/refresh.png")),
            settings: load(ctx, "settings", include_bytes!("../assets/icons/settings.png")),
        }
    }
}

/// What to draw in the icon slot of a toolbar button.
#[derive(Clone, Copy)]
pub enum Glyph<'a> {
    Tex(&'a TextureHandle),
    Drive,
    Stop,
    Shield,
    Size,
    Alloc,
}

pub const ICON: f32 = 20.0;
pub const H: f32 = 40.0;

/// Toolbar button: icon on top, label below. Hover lights it up, pressing pushes the
/// icon and label down 1.5 px so the click is felt, a selected toggle stays lit,
/// and disabled buttons are greyed out.
pub fn tool_button(ui: &mut Ui, glyph: Glyph, label: &str, enabled: bool, selected: bool) -> Response {
    let font = FontId::proportional(11.5);
    let text_w = ui
        .painter()
        .layout_no_wrap(label.to_owned(), font.clone(), Color32::WHITE)
        .size()
        .x;
    let w = (text_w + 10.0).max(38.0);
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, H), sense);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let lit = enabled && (resp.hovered() || pressed);
    if pressed || lit || selected {
        let (bg, line) = if pressed {
            (Color32::from_rgb(40, 74, 120), Color32::from_rgb(90, 150, 220))
        } else if selected {
            (Color32::from_rgb(30, 60, 100), Color32::from_rgb(70, 130, 200))
        } else {
            (Color32::from_gray(58), Color32::from_gray(85))
        };
        p.rect_filled(rect, 5.0, bg);
        p.rect_stroke(rect, 5.0, Stroke::new(1.0, line), StrokeKind::Inside);
    }
    let push = if pressed { 1.5 } else { 0.0 };
    let icon_rect =
        Rect::from_center_size(rect.center_top() + vec2(0.0, 4.0 + ICON / 2.0 + push), vec2(ICON, ICON));
    paint_glyph(p, glyph, icon_rect, enabled);
    let color = if enabled { Color32::from_gray(225) } else { Color32::from_gray(110) };
    p.text(rect.center_bottom() - vec2(0.0, 8.0 - push), Align2::CENTER_CENTER, label, font, color);
    resp
}

pub fn paint_glyph(p: &Painter, glyph: Glyph, r: Rect, enabled: bool) {
    let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.35) };
    let s = r.width() / 20.0;
    let c = r.center();
    match glyph {
        Glyph::Tex(t) => {
            let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
            p.image(t.id(), r, uv, dim(Color32::WHITE));
        }
        Glyph::Drive => {
            let body = Rect::from_center_size(c + vec2(0.0, 1.0) * s, vec2(19.0, 11.0) * s);
            p.rect_filled(body, CornerRadius::same(2), dim(Color32::from_gray(190)));
            p.rect_stroke(body, 2.0, Stroke::new(1.0, dim(Color32::from_gray(90))), StrokeKind::Inside);
            let slit_y = body.top() + 3.5 * s;
            p.line_segment(
                [pos2(body.left() + 3.0 * s, slit_y), pos2(body.right() - 3.0 * s, slit_y)],
                Stroke::new(1.0, dim(Color32::from_gray(120))),
            );
            p.circle_filled(
                pos2(body.right() - 3.5 * s, body.bottom() - 3.0 * s),
                1.6 * s,
                dim(Color32::from_rgb(80, 220, 110)),
            );
        }
        Glyph::Stop => {
            let sq = Rect::from_center_size(c, vec2(14.0, 14.0) * s);
            p.rect_filled(sq, CornerRadius::same(3), dim(Color32::from_rgb(220, 70, 60)));
        }
        Glyph::Shield => {
            let pts = |k: f32| {
                vec![
                    c + vec2(0.0, -9.0) * s * k,
                    c + vec2(8.0, -6.0) * s * k,
                    c + vec2(7.0, 2.0) * s * k,
                    c + vec2(0.0, 9.0) * s * k,
                    c + vec2(-7.0, 2.0) * s * k,
                    c + vec2(-8.0, -6.0) * s * k,
                ]
            };
            p.add(Shape::convex_polygon(pts(1.0), dim(Color32::from_rgb(60, 120, 215)), Stroke::NONE));
            // right half in yellow, like the Windows UAC shield
            let half = vec![
                c + vec2(0.0, -9.0) * s,
                c + vec2(8.0, -6.0) * s,
                c + vec2(7.0, 2.0) * s,
                c + vec2(0.0, 9.0) * s,
            ];
            p.add(Shape::convex_polygon(half, dim(Color32::from_rgb(240, 200, 60)), Stroke::NONE));
            p.add(Shape::closed_line(pts(1.0), Stroke::new(1.0, dim(Color32::from_gray(30)))));
        }
        Glyph::Size => {
            // a page with a folded corner
            let page = Rect::from_center_size(c, vec2(13.0, 17.0) * s);
            let fold = 4.0 * s;
            let body = vec![
                page.left_top(),
                page.right_top() - vec2(fold, 0.0),
                page.right_top() + vec2(0.0, fold),
                page.right_bottom(),
                page.left_bottom(),
            ];
            p.add(Shape::convex_polygon(body.clone(), dim(Color32::from_gray(235)), Stroke::NONE));
            p.add(Shape::closed_line(body, Stroke::new(1.0, dim(Color32::from_gray(90)))));
            for k in 0..3 {
                let y = page.top() + (7.0 + 3.0 * k as f32) * s;
                p.line_segment(
                    [pos2(page.left() + 3.0 * s, y), pos2(page.right() - 3.0 * s, y)],
                    Stroke::new(1.0, dim(Color32::from_gray(150))),
                );
            }
        }
        Glyph::Alloc => {
            // disk blocks: a 3 x 3 grid, last block half used
            let cell = 5.0 * s;
            let gap = 1.5 * s;
            let origin = c - vec2(1.5 * cell + gap, 1.5 * cell + gap);
            for row in 0..3 {
                for col in 0..3 {
                    let min = origin + vec2(col as f32, row as f32) * (cell + gap);
                    let b = Rect::from_min_size(min, vec2(cell, cell));
                    let full = !(row == 2 && col == 2);
                    let fill = if full {
                        Color32::from_rgb(80, 150, 230)
                    } else {
                        Color32::from_gray(80)
                    };
                    p.rect_filled(b, 1.0, dim(fill));
                    if !full {
                        let half = Rect::from_min_size(min, vec2(cell / 2.0, cell));
                        p.rect_filled(half, 1.0, dim(Color32::from_rgb(80, 150, 230)));
                    }
                }
            }
        }
    }
}
