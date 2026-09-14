//! A toolbar menu of colour swatches for the editor: a named palette, a
//! choice that hands the text back to its surroundings, and a custom picker.
//! The swatch matching the colour under the caret wears a check.

use crate::accent::rgba_hex;
use crate::i18n::gettext;
use adw::prelude::*;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// (CSS hex, untranslated name); the GNOME HIG palette.
pub type Palette = &'static [(&'static str, &'static str)];

pub const TEXT_PALETTE: Palette = &[
    ("#1c71d8", "Blue"),
    ("#26a269", "Green"),
    ("#e5a50a", "Yellow"),
    ("#e66100", "Orange"),
    ("#c01c28", "Red"),
    ("#813d9c", "Purple"),
    ("#865e3c", "Brown"),
    ("#3d3846", "Dark"),
    ("#62a0ea", "Light Blue"),
    ("#57e389", "Light Green"),
    ("#f8e45c", "Light Yellow"),
    ("#ffbe6f", "Light Orange"),
    ("#ed333b", "Light Red"),
    ("#c061cb", "Light Purple"),
    ("#cdab8f", "Light Brown"),
    ("#9a9996", "Grey"),
];

pub const HIGHLIGHT_PALETTE: Palette = &[
    ("#f9f06b", "Yellow"),
    ("#8ff0a4", "Green"),
    ("#99c1f1", "Blue"),
    ("#ffbe6f", "Orange"),
    ("#f66151", "Red"),
    ("#dc8add", "Purple"),
    ("#cdab8f", "Brown"),
    ("#c0bfbc", "Grey"),
];

const COLUMNS: i32 = 8;
const SWATCH_SIZE: i32 = 22;

struct Swatch {
    hex: String,
    checked: Rc<Cell<bool>>,
    area: gtk::DrawingArea,
}

pub struct ColorMenu {
    popover: gtk::Popover,
    swatches: Vec<Swatch>,
    none_check: gtk::Image,
    custom_title: String,
    current: RefCell<Option<gdk::RGBA>>,
    on_pick: Rc<dyn Fn(Option<gdk::RGBA>)>,
}

impl ColorMenu {
    /// Hang the menu off `button`. `none_label` names the choice that clears
    /// the colour; `on_pick` gets the colour chosen, `None` for that choice.
    pub fn attach(
        button: &gtk::MenuButton,
        palette: Palette,
        none_label: &str,
        on_pick: impl Fn(Option<gdk::RGBA>) + 'static,
    ) -> Rc<Self> {
        let on_pick: Rc<dyn Fn(Option<gdk::RGBA>)> = Rc::new(on_pick);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        let popover = gtk::Popover::builder().child(&content).build();
        button.set_popover(Some(&popover));

        let grid = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(4)
            .build();
        content.append(&grid);
        let mut swatches = Vec::with_capacity(palette.len());
        for (i, &(hex, name)) in palette.iter().enumerate() {
            let Ok(rgba) = gdk::RGBA::parse(hex) else {
                continue;
            };
            let checked = Rc::new(Cell::new(false));
            let area = gtk::DrawingArea::builder()
                .content_width(SWATCH_SIZE)
                .content_height(SWATCH_SIZE)
                .build();
            area.set_draw_func(glib::clone!(
                #[strong]
                checked,
                move |_, cr, width, height| draw_swatch(cr, width, height, &rgba, checked.get())
            ));
            let swatch_button = gtk::Button::builder()
                .child(&area)
                .tooltip_text(gettext(name))
                .css_classes(["flat", "color-swatch"])
                .build();
            swatch_button.connect_clicked(glib::clone!(
                #[weak]
                popover,
                #[strong]
                on_pick,
                move |_| {
                    popover.popdown();
                    on_pick(Some(rgba));
                }
            ));
            let i = i as i32;
            grid.attach(&swatch_button, i % COLUMNS, i / COLUMNS, 1, 1);
            swatches.push(Swatch {
                hex: rgba_hex(&rgba),
                checked,
                area,
            });
        }

        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

        let none_check = gtk::Image::from_icon_name("object-select-symbolic");
        none_check.set_visible(false);
        let none_button = menu_row(none_label, Some(&none_check));
        none_button.connect_clicked(glib::clone!(
            #[weak]
            popover,
            #[strong]
            on_pick,
            move |_| {
                popover.popdown();
                on_pick(None);
            }
        ));
        content.append(&none_button);

        let custom_title = button
            .tooltip_text()
            .map(|t| t.to_string())
            .unwrap_or_else(|| gettext("Custom Colour"));
        let custom_button = menu_row(&gettext("Custom Colour…"), None);
        content.append(&custom_button);

        let this = Rc::new(ColorMenu {
            popover,
            swatches,
            none_check,
            custom_title,
            current: RefCell::new(None),
            on_pick,
        });
        let weak = Rc::downgrade(&this);
        custom_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.pick_custom();
            }
        });
        this
    }

    /// Mark `color` as the one under the caret (`None`: no colour set).
    pub fn set_current(&self, color: Option<gdk::RGBA>) {
        if *self.current.borrow() == color {
            return;
        }
        let hex = color.as_ref().map(rgba_hex);
        for swatch in &self.swatches {
            let checked = hex.as_deref() == Some(swatch.hex.as_str());
            if swatch.checked.replace(checked) != checked {
                swatch.area.queue_draw();
            }
        }
        self.none_check.set_visible(color.is_none());
        self.current.replace(color);
    }

    fn pick_custom(&self) {
        self.popover.popdown();
        let parent = self.popover.root().and_downcast::<gtk::Window>();
        let dialog = gtk::ColorDialog::builder()
            .title(&self.custom_title)
            .with_alpha(false)
            .build();
        let on_pick = self.on_pick.clone();
        dialog.choose_rgba(
            parent.as_ref(),
            self.current.borrow().as_ref(),
            gio::Cancellable::NONE,
            move |result| {
                if let Ok(rgba) = result {
                    on_pick(Some(rgba));
                }
            },
        );
    }
}

/// A flat, full-width menu entry with an optional trailing check.
fn menu_row(label: &str, check: Option<&gtk::Image>) -> gtk::Button {
    let row = gtk::Box::builder().spacing(6).build();
    row.append(
        &gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .hexpand(true)
            .build(),
    );
    if let Some(check) = check {
        row.append(check);
    }
    gtk::Button::builder()
        .child(&row)
        .css_classes(["flat"])
        .build()
}

fn draw_swatch(cr: &gtk::cairo::Context, width: i32, height: i32, rgba: &gdk::RGBA, checked: bool) {
    let (w, h) = (f64::from(width), f64::from(height));
    let radius = 5.0;
    let (x0, y0, x1, y1) = (0.5, 0.5, w - 0.5, h - 0.5);
    cr.new_sub_path();
    cr.arc(
        x1 - radius,
        y0 + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        0.0,
    );
    cr.arc(
        x1 - radius,
        y1 - radius,
        radius,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        x0 + radius,
        y1 - radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        x0 + radius,
        y0 + radius,
        radius,
        std::f64::consts::PI,
        3.0 * std::f64::consts::FRAC_PI_2,
    );
    cr.close_path();
    cr.set_source_rgb(
        f64::from(rgba.red()),
        f64::from(rgba.green()),
        f64::from(rgba.blue()),
    );
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.2);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    if !checked {
        return;
    }
    // A check in whichever of black and white reads on the swatch.
    let luminance = 0.299 * f64::from(rgba.red())
        + 0.587 * f64::from(rgba.green())
        + 0.114 * f64::from(rgba.blue());
    let ink = if luminance > 0.6 { 0.0 } else { 1.0 };
    cr.set_source_rgb(ink, ink, ink);
    cr.set_line_width(2.0);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.set_line_join(gtk::cairo::LineJoin::Round);
    cr.move_to(w * 0.28, h * 0.52);
    cr.line_to(w * 0.44, h * 0.68);
    cr.line_to(w * 0.72, h * 0.34);
    let _ = cr.stroke();
}
