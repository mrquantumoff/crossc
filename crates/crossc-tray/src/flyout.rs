//! The details window: a borderless GPUI popup next to the notification
//! area, one card per pair of buds, updated with every snapshot.
//!
//! It is a process of its own (`crossc-tray --flyout`), started by the
//! icon's process on a click and ended when the window loses the focus or
//! Esc is pressed. Why not a window of the icon's process: GPUI ends its
//! message loop when its last window closes, and while it runs it keeps a
//! thread that wakes at the display's refresh rate, and a GPU device. As a
//! process of its own the flyout costs that only while it is open, and
//! the icon stays plain Win32.
//!
//! This file only draws: what is shown comes from `view`.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{
    App, AppContext, Application, Bounds, Context, Div, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, Render, SharedString, Subscription, Task, TitlebarOptions, Window, WindowBounds,
    WindowKind, WindowOptions, div, point, prelude::*, px, rgb, size,
};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromRect,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GWL_STYLE, GetWindowRect, HWND_TOPMOST, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SetWindowLongPtrW, SetWindowPos, WS_POPUP, WS_VISIBLE,
};

use crate::args::Args;
use crate::view::{self, Card, Feed, Rect, Row, Section, Stat, Tone};
use crate::{demo, source};

mod color {
    // The installer's palette.
    pub const BG: u32 = 0x16181d;
    pub const PANEL: u32 = 0x1e2128;
    pub const BORDER: u32 = 0x2c313a;
    pub const TEXT: u32 = 0xd7dae0;
    pub const MUTED: u32 = 0x8b929e;
    pub const GOOD: u32 = 0x4cc38a;
    pub const WARN: u32 = 0xe5b04a;
    pub const WARN_BG: u32 = 0x2b2414;
    pub const BAD: u32 = 0xe5534b;
    pub const BAD_BG: u32 = 0x2e1a19;
    pub const BADGE: u32 = 0x2d3440;
}

/// Distance between the flyout and the edges of the work area, logical px.
const MARGIN: f32 = 12.;
/// The demo's states change at most this often.
const DEMO_TICK: Duration = Duration::from_millis(500);

fn tone_color(tone: Tone) -> u32 {
    match tone {
        Tone::Normal => color::TEXT,
        Tone::Muted => color::MUTED,
        Tone::Good => color::GOOD,
        Tone::Warn => color::WARN,
        Tone::Bad => color::BAD,
    }
}

// ---------------------------------------------------------------------------
// placement (Win32, physical pixels)
// ---------------------------------------------------------------------------

/// Where the window goes and how large it is, for a wanted logical size.
struct Placement {
    /// Physical pixels.
    rect: Rect,
    /// Physical pixels per logical pixel on that monitor.
    scale: f32,
}

fn rect_of(r: RECT) -> Rect {
    Rect {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// Next to `anchor` (the notification icon), or in the bottom right
/// corner of the primary monitor's work area when there is none.
fn placement(anchor: Option<Rect>, logical: (u32, u32)) -> Option<Placement> {
    // SAFETY: plain queries; the structures are sized as the API wants.
    unsafe {
        let monitor = match anchor {
            Some(a) => MonitorFromRect(
                &RECT {
                    left: a.left,
                    top: a.top,
                    right: a.right,
                    bottom: a.bottom,
                },
                MONITOR_DEFAULTTONEAREST,
            ),
            None => MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY),
        };
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return None;
        }
        let work = rect_of(info.rcWork);
        let (mut dpi_x, mut dpi_y) = (96u32, 96u32);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let scale = dpi_x.max(48) as f32 / 96.;
        // No icon known: as if it sat just below the corner.
        let anchor = anchor.unwrap_or(Rect {
            left: work.right - 1,
            top: work.bottom,
            right: work.right,
            bottom: work.bottom + 1,
        });
        let px_size = (
            (logical.0 as f32 * scale).round() as i32,
            (logical.1 as f32 * scale).round() as i32,
        );
        Some(Placement {
            rect: view::place(anchor, work, px_size, (MARGIN * scale).round() as i32),
            scale,
        })
    }
}

/// Move and size the window to `rect`. A move onto a monitor of another
/// DPI makes Windows (and GPUI) resize it once more; so check and repeat.
fn apply(hwnd: HWND, rect: Rect) {
    for _ in 0..3 {
        // SAFETY: plain calls on our window.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                rect.left,
                rect.top,
                rect.width(),
                rect.height(),
                SWP_NOACTIVATE,
            );
            let mut now = RECT::default();
            if GetWindowRect(hwnd, &mut now).is_ok() && rect_of(now) == rect {
                return;
            }
        }
    }
}

fn hwnd_of(window: &Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(HWND(h.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// Keeps the window as large as its content wants, and in its place.
///
/// Used from the thread the states come from, never from the window's own:
/// GPUI handles the resulting WM_SIZE by updating the app, which it cannot
/// do while the app is already being updated (as it is whenever the view
/// runs), and then the window is resized but its content is not. From
/// another thread the message is delivered when the window's thread is
/// back in its message loop.
struct Placer {
    /// The window, once it exists (0: not yet).
    hwnd: AtomicIsize,
    anchor: Option<Rect>,
    sizes: Mutex<Sizes>,
}

/// Logical sizes of the window.
struct Sizes {
    wanted: (u32, u32),
    /// The one the window was last given.
    applied: Option<(u32, u32)>,
}

impl Placer {
    fn new(anchor: Option<Rect>, initial: (u32, u32)) -> Placer {
        Placer {
            hwnd: AtomicIsize::new(0),
            anchor,
            sizes: Mutex::new(Sizes {
                wanted: initial,
                applied: None,
            }),
        }
    }

    /// The content wants `size` from now on.
    fn want(&self, size: (u32, u32)) {
        if let Ok(mut s) = self.sizes.lock() {
            s.wanted = size;
        }
        self.fit();
    }

    /// Give the window the wanted size, if it does not have it.
    fn fit(&self) {
        let hwnd = self.hwnd.load(Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }
        let Ok(mut s) = self.sizes.lock() else {
            return;
        };
        if s.applied == Some(s.wanted) {
            return;
        }
        if let Some(p) = placement(self.anchor, s.wanted) {
            apply(HWND(hwnd as *mut core::ffi::c_void), p.rect);
        }
        s.applied = Some(s.wanted);
    }
}

// ---------------------------------------------------------------------------
// the view
// ---------------------------------------------------------------------------

struct Flyout {
    feed: Feed,
    /// Stay open whatever happens to the focus (`--pin`).
    pinned: bool,
    /// The window has had the focus (it closes on losing it, not on never
    /// having had it).
    was_active: bool,
    focus: FocusHandle,
    _feed_task: Task<()>,
    _activation: Subscription,
}

impl Flyout {
    fn set_feed(&mut self, feed: Feed, cx: &mut Context<Self>) {
        if feed == self.feed {
            return;
        }
        self.feed = feed;
        cx.notify();
    }

    fn render_row(row: &Row) -> Div {
        let mut value = div().flex().flex_col().flex_1().min_w_0().child(
            div()
                .h(px(view::ROW as f32))
                .flex()
                .items_center()
                .overflow_hidden()
                .text_color(rgb(tone_color(row.tone)))
                .child(SharedString::from(row.value.clone())),
        );
        if let Some(detail) = &row.detail {
            value = value.child(
                div()
                    .h(px((view::DETAIL * view::detail_lines(row)) as f32))
                    .mt(px(-3.))
                    .mb(px(3.))
                    .overflow_hidden()
                    .text_xs()
                    .line_height(px(view::DETAIL as f32))
                    .text_color(rgb(color::MUTED))
                    .child(SharedString::from(detail.clone())),
            );
        }
        div()
            .flex()
            .items_start()
            .text_sm()
            .child(
                div()
                    .h(px(view::ROW as f32))
                    .w(px(122.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .text_color(rgb(color::MUTED))
                    .child(row.label),
            )
            .child(value)
    }

    fn render_section(section: &Section) -> Div {
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .h(px(view::SECTION_TITLE as f32))
                    .flex()
                    .items_end()
                    .pb(px(3.))
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(color::MUTED))
                    .child(section.title.to_uppercase()),
            )
            .children(section.rows.iter().map(Self::render_row))
    }

    fn render_note(tone: Tone, text: &str) -> Div {
        let (fg, bg) = match tone {
            Tone::Bad => (color::BAD, color::BAD_BG),
            _ => (color::WARN, color::WARN_BG),
        };
        div().pt(px(8.)).child(
            div()
                .h(px((view::CARD_NOTE - 8) as f32))
                .px_2()
                .flex()
                .items_center()
                .overflow_hidden()
                .rounded_md()
                .bg(rgb(bg))
                .text_xs()
                .line_height(px(16.))
                .text_color(rgb(fg))
                // A block of its own, so that the text wraps (a flex
                // item's text does not).
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .max_h(px(32.))
                        .overflow_hidden()
                        .child(SharedString::from(text.to_string())),
                ),
        )
    }

    fn render_card(card: &Card) -> Div {
        let mut body = div()
            .w(px(view::COLUMN_WIDTH as f32))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .p(px(12.))
            .rounded_lg()
            .bg(rgb(color::PANEL))
            .border_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .h(px(view::CARD_HEAD as f32))
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(24.))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .overflow_hidden()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(SharedString::from(card.title.clone())),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .px_2()
                                    .rounded_full()
                                    .bg(rgb(color::BADGE))
                                    .text_xs()
                                    .text_color(rgb(tone_color(card.badge_tone)))
                                    .child(SharedString::from(card.badge.clone())),
                            ),
                    )
                    .child(
                        div()
                            .h(px(20.))
                            .overflow_hidden()
                            .text_sm()
                            .text_color(rgb(color::MUTED))
                            .child(SharedString::from(card.summary.clone())),
                    ),
            );
        if !card.stats.is_empty() {
            body = body.child(
                div()
                    .h(px(view::CARD_STATS as f32))
                    .pt(px(8.))
                    .flex()
                    .gap(px(6.))
                    .children(card.stats.iter().map(Self::render_stat)),
            );
        }
        body = body.children(card.sections.iter().map(Self::render_section));
        if let Some((tone, text)) = &card.note {
            // At the bottom, also of a card stretched to its neighbour's
            // height.
            body = body.child(Self::render_note(*tone, text).mt_auto());
        }
        body
    }

    /// One of the large numbers at the top of a card.
    fn render_stat(stat: &Stat) -> Div {
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .justify_center()
            .px(px(8.))
            .overflow_hidden()
            .rounded_md()
            .bg(rgb(color::BG))
            .border_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .h(px(15.))
                    .overflow_hidden()
                    .text_size(px(10.5))
                    .line_height(px(15.))
                    .text_color(rgb(color::MUTED))
                    .child(stat.label.to_uppercase()),
            )
            .child(
                div()
                    .h(px(21.))
                    .overflow_hidden()
                    .text_sm()
                    .line_height(px(21.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(tone_color(stat.tone)))
                    .child(SharedString::from(stat.value.clone())),
            )
    }

    fn render_headline(tone: Tone, title: &str, text: &str) -> Div {
        div()
            .h(px(view::HEADLINE as f32))
            .flex()
            .flex_col()
            .gap_2()
            .p(px(14.))
            .overflow_hidden()
            .rounded_lg()
            .bg(rgb(color::PANEL))
            .border_1()
            .border_color(rgb(color::BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().size(px(10.)).rounded_full().bg(rgb(tone_color(tone))))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(title.to_string())),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(color::MUTED))
                    .child(SharedString::from(text.to_string())),
            )
    }
}

impl Render for Flyout {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cards = view::cards(&self.feed);
        let columns = (cards.len() as u32).clamp(1, view::MAX_COLUMNS) as usize;
        let mut body = div()
            .id("body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(view::GAP as f32));
        if let Some((tone, title, text)) = view::headline(&self.feed) {
            body = body.child(Self::render_headline(tone, title, &text));
        }
        for row in cards.chunks(columns) {
            body = body.child(
                // Not `items_start`: the cards of a row are all as high
                // as the highest.
                div()
                    .flex()
                    .gap(px(view::GAP as f32))
                    .children(row.iter().map(Self::render_card)),
            );
        }
        if let Some((tone, text)) = view::service_note(&self.feed) {
            body = body.child(Self::render_note(tone, &text));
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" && !this.pinned {
                    cx.quit();
                }
            }))
            .p(px(view::PADDING as f32))
            .bg(rgb(color::BG))
            .text_color(rgb(color::TEXT))
            .font_family("Segoe UI")
            .child(body)
            .child(
                div()
                    .h(px(view::FOOTER as f32))
                    .flex_shrink_0()
                    .flex()
                    .items_end()
                    .overflow_hidden()
                    .text_xs()
                    .text_color(rgb(color::MUTED))
                    .child(view::footer(&self.feed)),
            )
    }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn demo_feed(args: &Args, epoch: u64) -> Feed {
    let elapsed = unix_ms().saturating_sub(epoch);
    match &args.demo_state {
        Some(name) => demo::state(name, elapsed).unwrap_or_else(|| demo::feed_at(elapsed)),
        None => demo::feed_at(elapsed),
    }
}

/// Run the flyout until it closes. Returns the process's exit code.
pub fn run(args: Args) -> i32 {
    let (tx, rx) = async_channel::unbounded::<Feed>();
    let epoch = args.demo_epoch.unwrap_or_else(unix_ms);
    let initial = if args.demo {
        demo_feed(&args, epoch)
    } else {
        Feed::Connecting
    };

    let placer = Arc::new(Placer::new(args.anchor, view::flyout_size(&initial)));

    // Where the states come from. Either way a thread that sizes the
    // window for the state and sends it into the channel; the window's
    // task sleeps until something arrives.
    let deliver = {
        let placer = placer.clone();
        move |feed: Feed| {
            placer.want(view::flyout_size(&feed));
            tx.send_blocking(feed).is_ok()
        }
    };
    let _client = if args.demo {
        let args = args.clone();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(DEMO_TICK);
                if !deliver(demo_feed(&args, epoch)) {
                    return;
                }
            }
        });
        None
    } else {
        source::connect(args.pipe.clone(), move |feed| {
            deliver(feed);
        })
    };

    Application::new().run(move |cx: &mut App| {
        let wanted = view::flyout_size(&initial);
        let place = placement(args.anchor, wanted);
        // GPUI takes logical pixels; on the monitor the window is meant
        // for these are the physical ones over its scale.
        let bounds = match &place {
            Some(p) => Bounds::new(
                point(
                    px(p.rect.left as f32 / p.scale),
                    px(p.rect.top as f32 / p.scale),
                ),
                size(
                    px(p.rect.width() as f32 / p.scale),
                    px(p.rect.height() as f32 / p.scale),
                ),
            ),
            None => Bounds::centered(None, size(px(wanted.0 as f32), px(wanted.1 as f32)), cx),
        };
        let pinned = args.pin;
        let opened = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                // A system title bar, as far as GPUI is concerned: with a
                // hidden one it keeps a resize frame's width of the window
                // out of the client area. The caption itself is taken off
                // below.
                titlebar: Some(TitlebarOptions {
                    title: Some("crossc".into()),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                // A popup: no taskbar button, not resizable.
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                ..Default::default()
            },
            |window, cx| {
                let hwnd = hwnd_of(window);
                if let Some(hwnd) = hwnd {
                    let round = DWMWCP_ROUND;
                    // SAFETY: style and a 4-byte attribute of our window.
                    unsafe {
                        // No caption, no frame: the client area is the window.
                        SetWindowLongPtrW(hwnd, GWL_STYLE, (WS_POPUP | WS_VISIBLE).0 as isize);
                        let _ = SetWindowPos(
                            hwnd,
                            None,
                            0,
                            0,
                            0,
                            0,
                            SWP_FRAMECHANGED
                                | SWP_NOMOVE
                                | SWP_NOSIZE
                                | SWP_NOZORDER
                                | SWP_NOACTIVATE,
                        );
                        let _ = DwmSetWindowAttribute(
                            hwnd,
                            DWMWA_WINDOW_CORNER_PREFERENCE,
                            (&raw const round).cast(),
                            size_of_val(&round) as u32,
                        );
                    }
                    // From here on the window is kept in place and in size
                    // by the placer; the first time right away, in case the
                    // window did not come up exactly where it was asked to
                    // (another monitor, another scale).
                    placer.hwnd.store(hwnd.0 as isize, Ordering::SeqCst);
                    let placer = placer.clone();
                    std::thread::spawn(move || placer.fit());
                }
                cx.new(|cx| {
                    let focus = cx.focus_handle();
                    window.focus(&focus);
                    let activation =
                        cx.observe_window_activation(window, |this: &mut Flyout, window, cx| {
                            if window.is_window_active() {
                                this.was_active = true;
                            } else if this.was_active && !this.pinned {
                                cx.quit();
                            }
                        });
                    let feed_task = cx.spawn(async move |this, cx| {
                        while let Ok(feed) = rx.recv().await {
                            if this
                                .update(cx, |this: &mut Flyout, cx| this.set_feed(feed, cx))
                                .is_err()
                            {
                                break;
                            }
                        }
                    });
                    Flyout {
                        feed: initial,
                        pinned,
                        was_active: false,
                        focus,
                        _feed_task: feed_task,
                        _activation: activation,
                    }
                })
            },
        );
        match opened {
            Ok(handle) => {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            Err(_) => cx.quit(),
        }
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
    0
}
