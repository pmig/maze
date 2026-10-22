//! Column layout of the main window as a pure function of the window width.
//!
//! CONCEPT §8.2: spaces bar | folders 240 | list 360 | message (flexible, min 480) |
//! agent 380. Below 1400 px the agent column collapses to its strip, below 1000 px the
//! message moves under the list (Thunderbird's Classic view). Both thresholds are
//! fields of [`LayoutConfig`], so settings can change them later.

/// Fixed sizes and thresholds, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayoutConfig {
    /// Width of the slim spaces bar on the far left.
    pub spaces_bar: f32,
    /// Width of the collapsed agent strip.
    pub agent_strip: f32,
    /// Below this window width the agent column collapses to its strip.
    pub agent_collapse_below: f32,
    /// Below this window width the message moves under the list.
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

/// Where the message view goes.
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

/// The layout to render.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedLayout {
    /// Folder pane width, `None` when hidden.
    pub folder_pane: Option<f32>,
    /// Message list width. `None` means it flexes (message hidden or below).
    pub list: Option<f32>,
    /// Message view placement.
    pub message: MessagePlacement,
    /// Agent column.
    pub agent: AgentPlacement,
}

impl ResolvedLayout {
    /// Whether the agent column is expanded.
    pub fn agent_expanded(&self) -> bool {
        matches!(self.agent, AgentPlacement::Expanded(_))
    }
}

/// Whether the agent column is expanded at this width, given the user's choice.
pub fn agent_expanded(window_width: f32, prefs: &LayoutPrefs, config: &LayoutConfig) -> bool {
    prefs
        .agent_expanded
        .unwrap_or(window_width >= config.agent_collapse_below)
}

/// Resolves the column layout for a window width.
pub fn resolve(window_width: f32, prefs: &LayoutPrefs, config: &LayoutConfig) -> ResolvedLayout {
    let clamp = |width: f32, min: f32| width.clamp(min, config.max_column.max(min));
    let agent = if agent_expanded(window_width, prefs, config) {
        AgentPlacement::Expanded(clamp(prefs.agent, config.min_agent))
    } else {
        AgentPlacement::Strip
    };
    let message = if !prefs.message_visible {
        MessagePlacement::Hidden
    } else if window_width < config.stack_message_below {
        MessagePlacement::Below
    } else {
        MessagePlacement::Beside
    };
    let mut folder_pane = prefs
        .folder_pane_visible
        .then(|| clamp(prefs.folder_pane, config.min_folder_pane));
    let mut list =
        (message == MessagePlacement::Beside).then(|| clamp(prefs.list, config.min_list));

    // Keep the message at its minimum width: take space from the list first, then
    // from the folder pane, never below their minimums.
    if let Some(list_width) = list.as_mut() {
        let agent_width = match agent {
            AgentPlacement::Expanded(width) => width,
            AgentPlacement::Strip => config.agent_strip,
        };
        let fixed = config.spaces_bar + folder_pane.unwrap_or(0.) + *list_width + agent_width;
        let mut deficit = (fixed + config.min_message - window_width).max(0.);
        let take = deficit.min(*list_width - config.min_list);
        *list_width -= take;
        deficit -= take;
        if let Some(folder_width) = folder_pane.as_mut() {
            let take = deficit.min(*folder_width - config.min_folder_pane);
            *folder_width -= take;
        }
    }

    ResolvedLayout {
        folder_pane,
        list,
        message,
        agent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(width: f32, prefs: LayoutPrefs) -> ResolvedLayout {
        resolve(width, &prefs, &LayoutConfig::default())
    }

    #[test]
    fn default_widths_on_a_wide_window() {
        let l = layout(1680., LayoutPrefs::default());
        assert_eq!(l.folder_pane, Some(240.));
        assert_eq!(l.list, Some(360.));
        assert_eq!(l.message, MessagePlacement::Beside);
        assert_eq!(l.agent, AgentPlacement::Expanded(380.));
    }

    #[test]
    fn agent_collapses_below_1400() {
        assert!(layout(1400., LayoutPrefs::default()).agent_expanded());
        let l = layout(1399., LayoutPrefs::default());
        assert_eq!(l.agent, AgentPlacement::Strip);
        assert_eq!(l.message, MessagePlacement::Beside);
    }

    #[test]
    fn message_moves_below_the_list_under_1000() {
        assert_eq!(
            layout(1000., LayoutPrefs::default()).message,
            MessagePlacement::Beside
        );
        let l = layout(999., LayoutPrefs::default());
        assert_eq!(l.message, MessagePlacement::Below);
        assert_eq!(l.list, None, "the list flexes when the message is below");
    }

    #[test]
    fn user_choice_overrides_the_agent_threshold() {
        let open = LayoutPrefs {
            agent_expanded: Some(true),
            ..LayoutPrefs::default()
        };
        assert!(layout(1100., open).agent_expanded());
        let closed = LayoutPrefs {
            agent_expanded: Some(false),
            ..LayoutPrefs::default()
        };
        assert!(!layout(2000., closed).agent_expanded());
    }

    #[test]
    fn toggles_hide_columns() {
        let l = layout(
            1680.,
            LayoutPrefs {
                folder_pane_visible: false,
                message_visible: false,
                ..LayoutPrefs::default()
            },
        );
        assert_eq!(l.folder_pane, None);
        assert_eq!(l.message, MessagePlacement::Hidden);
        assert_eq!(l.list, None);
    }

    #[test]
    fn keeps_the_message_at_its_minimum_by_shrinking_list_then_folders() {
        // 1100 px, agent strip: 44 + 240 + 360 + 40 = 684 fixed, 416 left < 480.
        let l = layout(1100., LayoutPrefs::default());
        let config = LayoutConfig::default();
        let used =
            config.spaces_bar + l.folder_pane.unwrap() + l.list.unwrap() + config.agent_strip;
        assert!((1100. - used - config.min_message).abs() < 0.01);
        assert_eq!(l.folder_pane, Some(240.));
        assert_eq!(l.list, Some(296.));

        // Narrower still: the list stops at its minimum, then folders shrink.
        let l = layout(1000., LayoutPrefs::default());
        assert_eq!(l.list, Some(config.min_list));
        assert_eq!(l.folder_pane, Some(176.));
    }

    #[test]
    fn widths_are_clamped() {
        let l = layout(
            3000.,
            LayoutPrefs {
                folder_pane: 10.,
                list: 5000.,
                agent: 0.,
                ..LayoutPrefs::default()
            },
        );
        let config = LayoutConfig::default();
        assert_eq!(l.folder_pane, Some(config.min_folder_pane));
        assert_eq!(l.list, Some(config.max_column));
        assert_eq!(l.agent, AgentPlacement::Expanded(config.min_agent));
    }
}
