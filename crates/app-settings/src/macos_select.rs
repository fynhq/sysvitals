//! macos_select: split out of the crate root (see `main.rs` for the module map).

use super::*;

// ---------------------------------------------------------------------------
// Native macOS dropdown menus
// ---------------------------------------------------------------------------

/// A system menu for the three string dropdowns on macOS.
///
/// The GPUI `Select` popover is a deferred element inside the settings
/// window, so it can never blur the desktop behind it: a translucent surface
/// would reveal the settings rows underneath, not the window material. Native
/// menus own their own vibrant window, which is the platform-correct way to
/// get the blur (and the keyboard/dismiss behavior) users expect from a
/// dropdown on macOS.
use std::cell::Cell;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, sel, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAppearanceCustomization, NSApplication, NSControlStateValueOn, NSEvent, NSMenu, NSMenuItem,
    NSView,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

thread_local! {
    static MENU_SELECTION: Cell<Option<isize>> = const { Cell::new(None) };
    static MENU_OPEN: Cell<bool> = const { Cell::new(false) };
}

/// Keeps a second popup from nesting inside an already tracking menu
/// (two fast clicks can defer two popups). Reset on every exit path.
pub(crate) struct MenuGuard;

impl MenuGuard {
    pub(crate) fn acquire() -> Option<Self> {
        MENU_OPEN.with(|open| (!open.replace(true)).then_some(Self))
    }
}

impl Drop for MenuGuard {
    fn drop(&mut self) {
        MENU_OPEN.with(|open| open.set(false));
    }
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SysVitalsSettingsMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(selectItem:))]
        fn select_item(&self, sender: Option<&NSMenuItem>) {
            if let Some(item) = sender {
                MENU_SELECTION.with(|slot| slot.set(Some(item.tag())));
            }
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    pub(crate) fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm);
        // SAFETY: MenuTarget does not override init; this dispatches to
        // NSObject's init, avoiding the super() wrapper's RetainSemantics
        // issues with Allocated<T> in objc2 0.6.5.
        unsafe { msg_send![this, init] }
    }
}

/// Show `items` at the pointer as a native menu and return the chosen
/// index. The current item carries the system checkmark. `None` means the
/// menu was dismissed without a selection, another menu is already open,
/// or no window was available.
pub(crate) fn show(items: &[String], selected: usize) -> Option<usize> {
    if items.is_empty() {
        return None;
    }
    let _guard = MenuGuard::acquire()?;
    let mtm = MainThreadMarker::new()?;
    let app = NSApplication::sharedApplication(mtm);
    let Some(window) = app.keyWindow().or_else(|| app.mainWindow()) else {
        crate::log_event("WARN", "settings: native select menu has no window");
        return None;
    };
    let Some(view) = window.contentView() else {
        crate::log_event("WARN", "settings: native select menu has no content view");
        return None;
    };

    let menu = NSMenu::new(mtm);
    // Every item has an explicit target/action below; automatic enabling
    // would otherwise walk the responder chain and gray them out.
    menu.setAutoenablesItems(false);
    // A menu raised from a vibrant window can inherit the button's
    // appearance and render dark in light mode. Follow the app's
    // effective appearance, like the indicator's context menu.
    let appearance = app.effectiveAppearance();
    menu.setAppearance(Some(&appearance));

    let target = MenuTarget::new(mtm);
    let target_ref: &MenuTarget = &target;
    for (index, label) in items.iter().enumerate() {
        let item = NSMenuItem::new(mtm);
        item.setTitle(&NSString::from_str(label));
        // SAFETY: the selector is implemented by MenuTarget above.
        unsafe { item.setAction(Some(sel!(selectItem:))) };
        // SAFETY: target_ref stays alive through the nested tracking loop.
        unsafe { item.setTarget(Some(target_ref)) };
        item.setTag(index as isize);
        if index == selected {
            item.setState(NSControlStateValueOn);
        }
        menu.addItem(&item);
    }

    // `mouseLocation` is in screen coordinates; convert through the
    // window into its content view, the space popUpMenuPositioningItem
    // expects. The menu's upper-left lands at the click point, like a
    // context menu raised from the trigger.
    let screen_point = NSEvent::mouseLocation();
    let window_point = window.convertPointFromScreen(screen_point);
    let view_point = view.convertPoint_fromView(window_point, None);
    let view_ref: &NSView = &view;

    MENU_SELECTION.with(|slot| slot.set(None));
    let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, view_point, Some(view_ref));
    MENU_SELECTION
        .with(|slot| slot.take())
        .and_then(|tag| usize::try_from(tag).ok())
}

#[cfg(target_os = "macos")]
#[derive(Copy, Clone)]
pub(crate) struct MacSelectStyle {
    pub(crate) background: Hsla,
    pub(crate) foreground: Hsla,
    pub(crate) border: Hsla,
    pub(crate) caret: Hsla,
    pub(crate) radius: Pixels,
}

/// Raise the native menu for a macOS dropdown and commit the chosen index.
///
/// `NSMenu` popup runs a nested AppKit tracking loop. Starting it inside a
/// GPUI click handler holds the mutable `App` borrow for the whole time the
/// menu is up, and the tracking loop keeps servicing the main run loop: the
/// 500 ms accent poll and the platform's keyboard-layout notification both
/// borrow the app from that loop and abort the process with "RefCell already
/// borrowed". Defer the popup by one main-queue turn so the click dispatch
/// has returned and no GPUI state is borrowed while the menu tracks; the
/// selection is applied afterwards.
#[cfg(target_os = "macos")]
pub(crate) fn mac_select_popup(
    cx: &mut Context<SettingsView>,
    items: Vec<String>,
    selected: usize,
    apply: fn(&mut SettingsView, usize),
) {
    cx.spawn(async move |this, cx| {
        // Must be non-zero: `BackgroundExecutor::timer` short-circuits a zero
        // duration to a ready task, which would keep this poll inline in the
        // click handler. A real timer is dispatched back through the platform
        // dispatcher and therefore lands after the click dispatch.
        cx.background_executor()
            .timer(std::time::Duration::from_millis(1))
            .await;
        let Some(index) = macos_select::show(&items, selected) else {
            return;
        };
        let _ = this.update(cx, |this, cx| {
            apply(this, index);
            this.save(cx);
            cx.notify();
        });
    })
    .detach();
}

/// The macOS dropdown trigger: the same filled control look as the GPUI
/// Select, but clicking it raises a native menu instead of an in-window
/// popover. Width is owned by the caller so all three dropdowns keep their
/// existing layout slots.
#[cfg(target_os = "macos")]
pub(crate) fn mac_select_trigger(
    id: &'static str,
    label: String,
    style: MacSelectStyle,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .w_full()
        .h(px(32.))
        .px(px(10.))
        .items_center()
        .justify_between()
        .gap_1()
        .bg(style.background)
        .text_color(style.foreground)
        .border_1()
        .border_color(style.border)
        .rounded(style.radius)
        .cursor_pointer()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .truncate()
                .child(label),
        )
        .child(Icon::new(IconName::ChevronDown).text_color(style.caret))
        .on_click(on_click)
}
