//! Small shared components that gpui-kit doesn't provide in the shape Maze needs.

use gpui::{
    App, ElementId, ImageSource, InteractiveElement as _, IntoElement, ObjectFit, ParentElement,
    Pixels, RenderOnce, SharedString, Styled, StyledImage as _, Window, div, img,
    prelude::FluentBuilder as _,
};
use gpui_kit::component::{ActiveTheme as _, Icon, StyledExt as _, h_flex};

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

/// Initials for an avatar: "Anna Example" → "AE", "bob@example.org" → "B" (the
/// name, else the part of the address before the `@`); "?" without letters.
pub fn initials(name: Option<&str>, email: &str) -> SharedString {
    let source = match name.map(str::trim) {
        Some(name) if !name.is_empty() => name,
        _ => email.split('@').next().unwrap_or_default(),
    };
    let letters: String = source
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".into()
    } else {
        letters.into()
    }
}

/// How an [`Avatar`] is colored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AvatarTone {
    /// The accent color, lightly (a person).
    #[default]
    Accent,
    /// On an accent background (a selected chip).
    OnAccent,
    /// The danger color (a recipient that isn't an address).
    Danger,
}

/// The round picture of a person: their initials on a tinted circle, or a photo.
///
/// The one look for people across Maze: recipient chips and suggestions, the Address
/// Book's rows and detail pane. Contact photos (and opt-in Gravatar, PLAN-contacts §3)
/// come through [`Avatar::photo`] once there is an avatar provider; until then
/// everyone gets initials.
#[derive(IntoElement)]
#[allow(missing_debug_implementations)] // `ImageSource` isn't `Debug`.
pub struct Avatar {
    initials: SharedString,
    size: Pixels,
    tone: AvatarTone,
    photo: Option<ImageSource>,
}

impl Avatar {
    /// An avatar of `size` showing `initials` (see [`initials`]).
    pub fn new(initials: impl Into<SharedString>, size: Pixels) -> Self {
        Self {
            initials: initials.into(),
            size,
            tone: AvatarTone::Accent,
            photo: None,
        }
    }

    /// The colors.
    pub fn tone(mut self, tone: AvatarTone) -> Self {
        self.tone = tone;
        self
    }

    /// A photo to show instead of the initials, if there is one.
    pub fn photo(mut self, photo: Option<ImageSource>) -> Self {
        self.photo = photo;
        self
    }
}

impl RenderOnce for Avatar {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        let (bg, fg) = match self.tone {
            AvatarTone::OnAccent => (
                theme.primary_foreground.opacity(0.25),
                theme.primary_foreground,
            ),
            AvatarTone::Danger => (theme.danger.opacity(0.15), theme.danger),
            AvatarTone::Accent => (theme.primary.opacity(0.2), theme.primary),
        };
        let circle = div()
            .size(self.size)
            .flex_none()
            .rounded_full()
            .overflow_hidden();
        match self.photo {
            Some(photo) => circle.child(
                img(photo)
                    .size(self.size)
                    .rounded_full()
                    .object_fit(ObjectFit::Cover),
            ),
            None => circle
                .bg(bg)
                .text_color(fg)
                .text_size((self.size * 0.45).round())
                .font_semibold()
                .flex()
                .items_center()
                .justify_center()
                .child(self.initials),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_come_from_the_name_or_the_address() {
        assert_eq!(
            initials(Some("Anna Example"), "anna@example.com").as_ref(),
            "AE"
        );
        assert_eq!(initials(Some("  "), "bob@example.org").as_ref(), "B");
        assert_eq!(initials(None, "bob@example.org").as_ref(), "B");
        assert_eq!(initials(Some("élise de la Tour"), "x@y").as_ref(), "ÉD");
        assert_eq!(initials(Some("(+)"), "").as_ref(), "?");
    }

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
