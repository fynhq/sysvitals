//! menu: split out of `macos` (see `macos/mod.rs` for the module map).

use super::*;

/// Show the context menu at the status item position.
pub fn show_context_menu() {
    let mtm = mtm();
    let target = ACTION_HANDLER.with(|h| h.borrow().clone());
    let Some(target) = target.as_ref() else {
        return;
    };
    STATUS_ITEM.with(|si| {
        let si_borrow = si.borrow();
        let Some(item) = si_borrow.as_ref() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };

        let menu = NSMenu::new(mtm);
        // A menu popped from a status-item button can inherit the button's
        // vibrant menu-bar appearance, which renders dark in light mode.
        // Follow the app's effective appearance so the menu matches the
        // system like native menu-extra menus.
        let app = NSApplication::sharedApplication(mtm);
        let appearance = app.effectiveAppearance();
        menu.setAppearance(Some(&appearance));
        // Menu labels follow the configured language, like the Windows
        // overlay's tray menu (previously hardcoded English here).
        let lang = CONFIG.with(|c| c.borrow().display.language);

        // Activity Monitor first: it used to be the double-click action,
        // and moving it to the top of the context menu is what freed the
        // left click to open the trend chart instantly (mirrors the
        // Windows overlay's Task Manager entry).
        let activity_title = NSString::from_str(t(lang, "menu.activity_monitor"));
        let activity_item = NSMenuItem::new(mtm);
        activity_item.setTitle(&activity_title);
        // SAFETY: setting a valid selector on a menu item.
        unsafe { activity_item.setAction(Some(sel!(showActivityMonitor:))) };
        // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
        unsafe { activity_item.setTarget(Some(target)) };
        menu.addItem(&activity_item);

        let settings_title = NSString::from_str(t(lang, "menu.settings"));
        let settings_item = NSMenuItem::new(mtm);
        settings_item.setTitle(&settings_title);
        unsafe { settings_item.setAction(Some(sel!(showSettings:))) };
        // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
        unsafe { settings_item.setTarget(Some(target)) };
        menu.addItem(&settings_item);

        let restart_title = NSString::from_str(t(lang, "menu.restart"));
        let restart_item = NSMenuItem::new(mtm);
        restart_item.setTitle(&restart_title);
        unsafe { restart_item.setAction(Some(sel!(restartApp:))) };
        // SAFETY: `target` is the live ActionHandler kept in ACTION_HANDLER.
        unsafe { restart_item.setTarget(Some(target)) };

        // Separator, then Restart above Quit: Restart is a lifecycle
        // action like Quit, so both sit below the divider, Restart first.
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        menu.addItem(&restart_item);

        let quit_title = NSString::from_str(t(lang, "menu.quit"));
        let quit_item = NSMenuItem::new(mtm);
        quit_item.setTitle(&quit_title);
        unsafe { quit_item.setAction(Some(sel!(quitApp:))) };
        unsafe { quit_item.setTarget(Some(target)) };
        menu.addItem(&quit_item);

        // Pop the menu directly at the button's lower-left corner. The
        // previous approach (attach with `setMenu`, then `performClick`,
        // then detach) is unreliable once the item lives in the notch
        // overflow panel: `performClick` there highlights the button but
        // does not pop the attached menu, so a right-click looked like it
        // did nothing. `popUpMenuPositioningItem` shows the menu against
        // the button view regardless of how the item is positioned, and it
        // runs a nested tracking loop that returns once the menu closes.
        let view: &NSView = &button;
        let location = NSPoint::new(0.0, button.bounds().size.height);
        let _ = menu.popUpMenuPositioningItem_atLocation_inView(None, location, Some(view));
    });
}
