//! QR codes for pairing addresses: ours rendered for another device to scan,
//! a peer's scanned with the camera (Android only; desktops rarely have one
//! pointed anywhere useful).

use crate::app::AppCtx;
use crate::{Logic, State};
use slint::{ComponentHandle, Image, Rgb8Pixel, SharedPixelBuffer};

/// Blank modules around the code; scanners need the margin.
const QUIET: u32 = 4;

/// `text` as a QR code, one pixel per module (the UI scales it up with
/// pixelated rendering).
pub fn render(text: &str) -> Option<SharedPixelBuffer<Rgb8Pixel>> {
    let code = qrcode::QrCode::new(text.as_bytes()).ok()?;
    let modules = code.width() as u32;
    let colors = code.to_colors();
    let side = modules + 2 * QUIET;
    let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(side, side);
    for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
        let (x, y) = (i as u32 % side, i as u32 / side);
        let inside = (QUIET..QUIET + modules).contains(&x) && (QUIET..QUIET + modules).contains(&y);
        let dark = inside
            && colors[((y - QUIET) * modules + (x - QUIET)) as usize] == qrcode::Color::Dark;
        let v = if dark { 0 } else { 255 };
        *px = Rgb8Pixel { r: v, g: v, b: v };
    }
    Some(buf)
}

pub(crate) fn register(ctx: &AppCtx) {
    let Some(ui) = ctx.weak.upgrade() else {
        return;
    };
    let logic = ui.global::<Logic>();
    {
        let weak = ctx.weak.clone();
        logic.on_toggle_my_qr(move || {
            let Some(ui) = weak.upgrade() else { return };
            let state = ui.global::<State>();
            let show = !state.get_show_qr();
            if show {
                match render(&state.get_my_ticket()) {
                    Some(buf) => state.set_my_qr(Image::from_rgb8(buf)),
                    None => return,
                }
            }
            state.set_show_qr(show);
        });
    }
    {
        let weak = ctx.weak.clone();
        logic.on_scan_qr(move || {
            let Some(ui) = weak.upgrade() else { return };
            let state = ui.global::<State>();
            state.set_scan_frame(Image::default());
            state.set_scanning(true);
            #[cfg(target_os = "android")]
            scan::start();
        });
    }
    {
        let weak = ctx.weak.clone();
        logic.on_scan_cancel(move || {
            if let Some(ui) = weak.upgrade() {
                stop_scan(&ui);
            }
        });
    }
    #[cfg(target_os = "android")]
    scan::register(ctx);
}

/// Close the scanner (and the camera) if it is open.
pub fn stop_scan(ui: &crate::AppWindow) {
    let state = ui.global::<State>();
    if state.get_scanning() {
        state.set_scanning(false);
        crate::android::stop_qr_camera();
    }
}

#[cfg(target_os = "android")]
mod scan {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set once a frame decoded, so frames still in flight are ignored.
    static FOUND: AtomicBool = AtomicBool::new(false);

    pub fn start() {
        FOUND.store(false, Ordering::SeqCst);
        crate::android::start_qr_camera();
    }

    pub fn register(ctx: &AppCtx) {
        let weak = ctx.weak.clone();
        let weak_stopped = ctx.weak.clone();
        crate::android::on_qr_camera(
            move |luma, width, height, rotation| {
                if FOUND.load(Ordering::SeqCst) {
                    return;
                }
                let found = decode(&luma, width, height);
                if found.is_some() && FOUND.swap(true, Ordering::SeqCst) {
                    return;
                }
                let frame = viewfinder(&luma, width, height, rotation);
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let state = ui.global::<State>();
                    if !state.get_scanning() {
                        return;
                    }
                    state.set_scan_frame(Image::from_rgb8(frame));
                    if let Some(text) = found {
                        stop_scan(&ui);
                        state.set_add_ticket_input(text.trim().into());
                        ui.global::<Logic>().invoke_pair_with_pasted();
                    }
                });
            },
            move |denied| {
                let _ = weak_stopped.upgrade_in_event_loop(move |ui| {
                    let state = ui.global::<State>();
                    if !state.get_scanning() {
                        return;
                    }
                    state.set_scanning(false);
                    let msg = if denied {
                        "Camera permission is needed to scan a QR code"
                    } else {
                        "Could not use the camera"
                    };
                    crate::platform::toast(&ui, msg, true);
                });
            },
        );
    }

    /// The text of the first QR code in a greyscale frame.
    fn decode(luma: &[u8], width: u32, height: u32) -> Option<String> {
        let (w, h) = (width as usize, height as usize);
        if luma.len() < w * h {
            return None;
        }
        let mut image = rqrr::PreparedImage::prepare_from_greyscale(w, h, |x, y| luma[y * w + x]);
        image
            .detect_grids()
            .into_iter()
            .find_map(|grid| grid.decode().ok().map(|(_, text)| text))
    }

    /// The frame at half size, turned upright by `rotation` degrees clockwise.
    fn viewfinder(luma: &[u8], width: u32, height: u32, rotation: u32) -> SharedPixelBuffer<Rgb8Pixel> {
        let (sw, sh) = (width / 2, height / 2);
        let sample = |x: u32, y: u32| luma[(2 * y * width + 2 * x) as usize];
        let (dw, dh) = if rotation % 180 == 90 { (sh, sw) } else { (sw, sh) };
        let mut buf = SharedPixelBuffer::<Rgb8Pixel>::new(dw, dh);
        for (i, px) in buf.make_mut_slice().iter_mut().enumerate() {
            let (dx, dy) = (i as u32 % dw, i as u32 / dw);
            let v = match rotation {
                90 => sample(dy, sh - 1 - dx),
                180 => sample(sw - 1 - dx, sh - 1 - dy),
                270 => sample(sw - 1 - dy, dx),
                _ => sample(dx, dy),
            };
            *px = Rgb8Pixel { r: v, g: v, b: v };
        }
        buf
    }
}
