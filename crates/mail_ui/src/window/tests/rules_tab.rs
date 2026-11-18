//! The rules tab: "Rules: Show Rules" lists the rules, a click toggles one and a
//! rule Maze can't run can't be enabled.

use gpui::{Modifiers, TestAppContext};

use super::*;
use crate::actions::ShowRules;
use crate::rules_tab::RulesTab;

#[gpui::test]
fn show_rules_lists_and_toggles_rules(cx: &mut TestAppContext) {
    let (view, cx) = open(cx);
    cx.dispatch_action(ShowRules);
    cx.run_until_parked();
    let tab = view.read_with(cx, |w, cx| {
        w.panes()
            .read(cx)
            .items_of::<RulesTab>(cx)
            .into_iter()
            .next()
            .expect("a rules tab")
    });
    let enabled = |cx: &mut gpui::VisualTestContext| -> Vec<bool> {
        tab.read_with(cx, |tab, _| tab.rules().iter().map(|r| r.enabled).collect())
    };
    assert_eq!(enabled(cx), [true, false, false]);

    // Enable the imported rule with its button.
    let bounds = cx.debug_bounds("rule-toggle-2").expect("the toggle");
    cx.simulate_click(bounds.center(), Modifiers::none());
    cx.run_until_parked();
    assert_eq!(enabled(cx), [true, true, false]);

    // The rule Maze can't run stays off.
    tab.update(cx, |tab, cx| tab.set_enabled(3, true, cx));
    assert_eq!(enabled(cx), [true, true, false]);
    assert!(tab.read_with(cx, |tab, _| tab.error().is_some()));

    // Showing the rules again activates the same tab.
    cx.dispatch_action(ShowRules);
    cx.run_until_parked();
    let tabs = view.read_with(cx, |w, cx| {
        w.panes().read(cx).items_of::<RulesTab>(cx).len()
    });
    assert_eq!(tabs, 1);
}
