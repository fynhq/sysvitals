//! Native macOS menu for string dropdowns.

use super::*;

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
    menu.setAutoenablesItems(false);
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

#[cfg(target_os = "macos")]
pub(crate) fn mac_select_popup(
    cx: &mut Context<SettingsView>,
    items: Vec<String>,
    selected: usize,
    apply: fn(&mut SettingsView, usize),
) {
    cx.spawn(async move |this, cx| {
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

#[cfg(target_os = "macos")]
pub(crate) fn mac_select_trigger(
    id: &'static str,
    label: String,
    a11y_label: String,
    style: MacSelectStyle,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    h_flex()
        .id(id)
        .role(Role::Button)
        .aria_label(a11y_label)
        .aria_value(label.clone())
        .aria_expanded(false)
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
