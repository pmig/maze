//! The agent column (CONCEPT §8.2 column 4, §10; PLAN §4.12) — layer L3.
//!
//! [`AgentPanel`] is the column's content: the agent picker with install progress, the
//! conversation of the running session (streamed Markdown answers, collapsed tool calls,
//! plans, terminal commands, inline permission prompts), sign-in, the history of
//! conversations about the selected mail thread, and the review of agent proposals.
//!
//! The main window (`mail_ui`) doesn't depend on this crate: the `mazemail` binary
//! creates the panel and hands it to the window as a view, and tells it the selected
//! thread ([`AgentPanel::set_context`]) and when proposals changed
//! ([`AgentPanel::refresh_proposals`]).
//!
//! Keys (`assets/keymaps/default-macos.json`, context `AgentPanel`): `cmd-n` new
//! thread, `cmd-shift-enter` / `cmd-shift-backspace` accept / reject all proposals, and
//! `y` / `n` for the highlighted proposal item while the review has focus. In the prompt,
//! `enter` sends and `shift-enter` starts a new line.

mod panel;

pub use panel::{AgentPanel, MailContext};

use gpui::actions;

actions!(
    agent,
    [
        /// Expand or collapse the agent column (the app forwards it to the window's
        /// `workspace::ToggleAgentColumn`, `cmd-?`).
        ToggleAgentPanel,
        /// End the current agent conversation and start a new one (`cmd-n`).
        NewThread,
        /// Show the conversations about the selected thread.
        ShowHistory,
        /// Move the focus to the agent prompt.
        FocusPrompt,
        /// Accept every pending proposal (`cmd-shift-enter`).
        AcceptAllProposals,
        /// Reject every pending proposal (`cmd-shift-backspace`).
        RejectAllProposals,
        /// Accept the highlighted proposal item (`y`).
        AcceptProposal,
        /// Reject the highlighted proposal item (`n`).
        RejectProposal,
        /// Highlight the next proposal item.
        NextProposal,
        /// Highlight the previous proposal item.
        PreviousProposal,
        /// Stop the agent's current turn.
        StopAgent,
    ]
);

#[cfg(test)]
mod tests;
