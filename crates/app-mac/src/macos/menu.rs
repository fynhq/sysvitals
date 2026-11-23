//! Status-item context menu construction and actions.

use super::*;

fn menu_item(
    mtm: MainThreadMarker,
    title: &str,
    action: objc2::runtime::Sel,
    target: &ActionHandler,
) -> Retained<NSMenuItem> {
    let item = NSMenuItem::new(mtm);
    item.setTitle(&NSString::from_str(title));
    // SAFETY: `action` is a selector implemented by `ActionHandler`;
    // `target` is the live handler kept in ACTION_HANDLER.
    unsafe {
        item.setAction(Some(action));
        item.setTarget(Some(target));
    }
    item
}

pub(crate) fn show_context_menu() {
    let mtm = mtm();
    let target = ACTION_HANDLER.with(|h| h.borrow().clone());
    let Some(target) = target.as_ref() else {
        return;
    };
    with_status_button(mtm, |button| {
        let menu = NSMenu::new(mtm);
        let app = NSApplication::sharedApplication(mtm);
        let appearance = app.effectiveAppearance();
        menu.setAppearance(Some(&appearance));
        let lang = CONFIG.with(|c| c.borrow().display.language);

        menu.addItem(&menu_item(
            mtm,
            t(lang, "menu.activity_monitor"),
            sel!(showActivityMonitor:),
            target,
        ));
        menu.addItem(&menu_item(
            mtm,
            t(lang, "menu.settings"),
            sel!(showSettings:),
            target,
        ));

        let restart_item = menu_item(mtm, t(lang, "menu.restart"), sel!(restartApp:), target);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&restart_item);
        menu.addItem(&menu_item(
            mtm,
            t(lang, "menu.quit"),
            sel!(quitApp:),
            target,
        ));

        let view: &NSView = button;
        let location = NSPoint::new(0.0, button.bounds().size.height);
        let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(view));
    });
}
