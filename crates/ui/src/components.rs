//! Small shared components that gpui-kit doesn't provide in the shape Maze needs.

use gpui::{
    App, ElementId, InteractiveElement as _, IntoElement, ParentElement, RenderOnce, SharedString,
    Styled, Window, div, prelude::FluentBuilder as _,
};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex};

/// An unread (or pending) count, right-aligned in a folder row or on a strip.
///
/// Zero renders nothing, like Thunderbird. Large counts are abbreviated (`12.3k`), so
/// a 1M-message folder doesn't widen the pane.
#[derive(Debug, IntoElement)]
pub struct CountBadge {
    count: u64,
    emphasized: bool,
}

impl CountBadge {
    /// A badge for `count`.
    pub fn new(count: u64) -> Self {
        Self {
            count,
            emphasized: false,
        }
    }

    /// Draw it on the accent color (for selected rows and attention badges).
    pub fn emphasized(mut self, emphasized: bool) -> Self {
        self.emphasized = emphasized;
        self
    }
}

/// Formats a count compactly: `999`, `1.2k`, `12k`, `1.2M`.
pub fn format_count(count: u64) -> SharedString {
    fn scaled(count: u64, unit: u64, suffix: &str) -> String {
        let tenths = count * 10 / unit;
        if tenths < 100 {
            let (whole, fraction) = (tenths / 10, tenths % 10);
            if fraction == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{fraction}{suffix}")
            }
        } else {
            format!("{}{suffix}", count / unit)
        }
    }
    match count {
        0..=999 => count.to_string().into(),
        1_000..=999_999 => scaled(count, 1_000, "k").into(),
        _ => scaled(count, 1_000_000, "M").into(),
    }
}

impl RenderOnce for CountBadge {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        div().when(self.count > 0, |this| {
            this.flex_none()
                .px_1p5()
                .rounded(theme.radius_full())
                .text_xs()
                .map(|this| {
                    if self.emphasized {
                        this.bg(theme.primary).text_color(theme.primary_foreground)
                    } else {
                        this.text_color(theme.muted_foreground)
                    }
                })
                .child(format_count(self.count))
        })
    }
}

/// One item in the status bar: an optional icon and a short label.
#[derive(IntoElement)]
#[allow(missing_debug_implementations)] // `Icon` isn't `Debug`.
pub struct StatusItem {
    id: ElementId,
    icon: Option<Icon>,
    label: SharedString,
}

impl StatusItem {
    /// A status item with a label.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            icon: None,
            label: label.into(),
        }
    }

    /// Show an icon before the label.
    pub fn icon(mut self, icon: impl Into<Icon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

impl RenderOnce for StatusItem {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        h_flex()
            .id(self.id)
            .gap_1()
            .items_center()
            .text_color(cx.theme().muted_foreground)
            .when_some(self.icon, |this, icon| this.child(icon.size_3()))
            .child(self.label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_compactly() {
        assert_eq!(format_count(0).as_ref(), "0");
        assert_eq!(format_count(999).as_ref(), "999");
        assert_eq!(format_count(1_000).as_ref(), "1k");
        assert_eq!(format_count(1_250).as_ref(), "1.2k");
        assert_eq!(format_count(12_345).as_ref(), "12k");
        assert_eq!(format_count(999_999).as_ref(), "999k");
        assert_eq!(format_count(1_000_000).as_ref(), "1M");
        assert_eq!(format_count(1_500_000).as_ref(), "1.5M");
    }
}
