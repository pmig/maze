//! Column layout of the main window as a pure function of the window width.
//!
//! CONCEPT §8.2: spaces bar | folders 240 | center | agent 380. The center is the pane
//! group (ADR-018); a mail view in it shows list 360 | message (flexible, min 480).
//! Below 1400 px the agent column collapses to its strip; a mail view narrower than
//! list and message at their minimums moves the message under the list
//! (Thunderbird's Classic view), which a single pane reaches below 1000 px. The
//! thresholds are fields of [`LayoutConfig`], so settings can change them later.

/// Fixed sizes and thresholds, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutConfig {
    /// Width of the slim spaces bar on the far left.
    pub spaces_bar: f32,
    /// Width of the collapsed agent strip.
    pub agent_strip: f32,
    /// Below this window width the agent column collapses to its strip.
    pub agent_collapse_below: f32,
    /// Below this window width the folder pane stops giving up width to keep the
    /// message beside the list.
    pub stack_message_below: f32,
    /// The message column never gets narrower than this while beside the list.
    pub min_message: f32,
    /// Minimum widths a resize can reach.
    pub min_folder_pane: f32,
    /// Minimum width of the message list.
    pub min_list: f32,
    /// Minimum width of the expanded agent column.
    pub min_agent: f32,
    /// Maximum width of any resizable side column.
    pub max_column: f32,
}

impl Default for LayoutConfig {
    fn default() -> Self {
        Self {
            spaces_bar: 44.,
            agent_strip: 40.,
            agent_collapse_below: 1400.,
            stack_message_below: 1000.,
            min_message: 480.,
            min_folder_pane: 160.,
            min_list: 260.,
            min_agent: 300.,
            max_column: 720.,
        }
    }
}

/// What the user chose: column widths and visibility toggles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutPrefs {
    /// Folder pane width.
    pub folder_pane: f32,
    /// Message list width (when the message is beside it).
    pub list: f32,
    /// Agent column width when expanded.
    pub agent: f32,
    /// `cmd-b` toggles this.
    pub folder_pane_visible: bool,
    /// `F8` toggles this.
    pub message_visible: bool,
    /// `cmd-?` sets this. `None` follows the window width.
    pub agent_expanded: Option<bool>,
}

impl Default for LayoutPrefs {
    fn default() -> Self {
        Self {
            folder_pane: 240.,
            list: 360.,
            agent: 380.,
            folder_pane_visible: true,
            message_visible: true,
            agent_expanded: None,
        }
    }
}

/// Where the message view goes in a mail view.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MessagePlacement {
    /// Beside the list, taking the remaining width.
    Beside,
    /// Under the list (narrow windows).
    Below,
    /// Hidden with `F8`; the list takes the space.
    Hidden,
}

/// The agent column's state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AgentPlacement {
    /// Full column of this width.
    Expanded(f32),
    /// The thin strip.
    Strip,
}

/// The window's columns to render; the center (the pane group) takes the rest.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLayout {
    /// Folder pane width, `None` when hidden.
    pub folder_pane: Option<f32>,
    /// Agent column.
    pub agent: AgentPlacement,
    /// The width left for the center.
    pub center: f32,
}

impl ResolvedLayout {
    /// Whether the agent column is expanded.
    pub fn agent_expanded(&self) -> bool {
        matches!(self.agent, AgentPlacement::Expanded(_))
    }
}

/// How a mail view (list and message) lays out in its pane.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MailViewLayout {
    /// Message list width. `None` means it flexes (message hidden or below).
    pub list: Option<f32>,
    /// Message view placement.
    pub message: MessagePlacement,
}

/// Whether the agent column is expanded at this width, given the user's choice.
pub fn agent_expanded(window_width: f32, prefs: &LayoutPrefs, config: &LayoutConfig) -> bool {
    prefs
        .agent_expanded
        .unwrap_or(window_width >= config.agent_collapse_below)
}

fn clamp(width: f32, min: f32, config: &LayoutConfig) -> f32 {
    width.clamp(min, config.max_column.max(min))
}

/// Resolves the window's columns for a window width. From
/// [`LayoutConfig::stack_message_below`] up, the folder pane gives up width (down to its
/// minimum) so a mail view in the center keeps the list and the message side by side.
pub fn resolve(window_width: f32, prefs: &LayoutPrefs, config: &LayoutConfig) -> ResolvedLayout {
    let agent = if agent_expanded(window_width, prefs, config) {
        AgentPlacement::Expanded(clamp(prefs.agent, config.min_agent, config))
    } else {
        AgentPlacement::Strip
    };
    let agent_width = match agent {
        AgentPlacement::Expanded(width) => width,
        AgentPlacement::Strip => config.agent_strip,
    };
    let mut folder_pane = prefs
        .folder_pane_visible
        .then(|| clamp(prefs.folder_pane, config.min_folder_pane, config));
    if prefs.message_visible
        && window_width >= config.stack_message_below
        && let Some(folder_width) = folder_pane.as_mut()
    {
        let center = window_width - config.spaces_bar - *folder_width - agent_width;
        let deficit = (config.min_list + config.min_message - center).max(0.);
        *folder_width -= deficit.min(*folder_width - config.min_folder_pane);
    }
    let center =
        (window_width - config.spaces_bar - folder_pane.unwrap_or(0.) - agent_width).max(0.);
    ResolvedLayout {
        folder_pane,
        agent,
        center,
    }
}

/// Lays out a mail view `width` wide: the message beside the list while both fit at
/// their minimum widths (the list gives up width first), else below it.
pub fn resolve_mail_view(width: f32, prefs: &LayoutPrefs, config: &LayoutConfig) -> MailViewLayout {
    if !prefs.message_visible {
        return MailViewLayout {
            list: None,
            message: MessagePlacement::Hidden,
        };
    }
    if width + 0.5 < config.min_list + config.min_message {
        return MailViewLayout {
            list: None,
            message: MessagePlacement::Below,
        };
    }
    let list = clamp(prefs.list, config.min_list, config).min(width - config.min_message);
    MailViewLayout {
        list: Some(list.max(config.min_list)),
        message: MessagePlacement::Beside,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: f32, prefs: LayoutPrefs) -> (ResolvedLayout, MailViewLayout) {
        let config = LayoutConfig::default();
        let outer = resolve(width, &prefs, &config);
        let view = resolve_mail_view(outer.center, &prefs, &config);
        (outer, view)
    }

    #[test]
    fn a_wide_window_uses_the_preferred_widths() {
        let (c, p) = (LayoutConfig::default(), LayoutPrefs::default());
        let (l, v) = layout(1680., p);
        assert_eq!(l.folder_pane, Some(p.folder_pane));
        assert_eq!(v.list, Some(p.list));
        assert_eq!(v.message, MessagePlacement::Beside);
        assert_eq!(l.agent, AgentPlacement::Expanded(p.agent));
        assert_eq!(l.center, 1680. - c.spaces_bar - p.folder_pane - p.agent);
    }

    #[test]
    fn thresholds_collapse_the_agent_then_stack_the_message() {
        let (c, p) = (LayoutConfig::default(), LayoutPrefs::default());
        assert!(layout(c.agent_collapse_below, p).0.agent_expanded());
        let (l, v) = layout(c.agent_collapse_below - 1., p);
        assert_eq!(l.agent, AgentPlacement::Strip);
        assert_eq!(v.message, MessagePlacement::Beside);

        assert_eq!(
            layout(c.stack_message_below, p).1.message,
            MessagePlacement::Beside
        );
        let (_, v) = layout(c.stack_message_below - 1., p);
        assert_eq!(v.message, MessagePlacement::Below);
        assert_eq!(v.list, None, "the list flexes when the message is below");

        // The user's choice overrides the agent threshold.
        let open = LayoutPrefs {
            agent_expanded: Some(true),
            ..p
        };
        assert!(layout(1100., open).0.agent_expanded());
        let closed = LayoutPrefs {
            agent_expanded: Some(false),
            ..p
        };
        assert!(!layout(2000., closed).0.agent_expanded());
    }

    #[test]
    fn the_message_keeps_its_minimum_by_shrinking_list_then_folders() {
        let (c, p) = (LayoutConfig::default(), LayoutPrefs::default());
        // A pane narrower than list and message at their minimums stacks the message.
        let both = c.min_list + c.min_message;
        assert_eq!(
            resolve_mail_view(both - 1., &p, &c).message,
            MessagePlacement::Below
        );
        let wide = resolve_mail_view(p.list + c.min_message + 60., &p, &c);
        assert_eq!(wide.message, MessagePlacement::Beside);
        assert_eq!(wide.list, Some(p.list));
        // The list gives way first.
        assert_eq!(
            resolve_mail_view(800., &p, &c).list,
            Some(800. - c.min_message)
        );

        // In the window (agent strip): the list shrinks first...
        let (l, v) = layout(1100., p);
        let center = 1100. - c.spaces_bar - p.folder_pane - c.agent_strip;
        assert_eq!(l.folder_pane, Some(p.folder_pane));
        assert_eq!(v.list, Some(center - c.min_message));
        // ...down to its minimum, then the folder pane.
        let (l, v) = layout(c.stack_message_below, p);
        assert_eq!(v.list, Some(c.min_list));
        assert_eq!(
            l.folder_pane,
            Some(c.stack_message_below - c.spaces_bar - c.agent_strip - both)
        );
    }

    #[test]
    fn widths_are_clamped_and_toggles_hide_columns() {
        let c = LayoutConfig::default();
        let (l, v) = layout(
            3000.,
            LayoutPrefs {
                folder_pane: 10.,
                list: 5000.,
                agent: 0.,
                ..LayoutPrefs::default()
            },
        );
        assert_eq!(l.folder_pane, Some(c.min_folder_pane));
        assert_eq!(v.list, Some(c.max_column));
        assert_eq!(l.agent, AgentPlacement::Expanded(c.min_agent));

        let (l, v) = layout(
            1680.,
            LayoutPrefs {
                folder_pane_visible: false,
                message_visible: false,
                ..LayoutPrefs::default()
            },
        );
        assert_eq!(l.folder_pane, None);
        assert_eq!(v.message, MessagePlacement::Hidden);
        assert_eq!(v.list, None);
    }
}
