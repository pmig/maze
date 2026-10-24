//! Actions of the composer (a tab of the main window, or a window of its own). Keys are bound in `assets/keymaps/default-macos.json`
//! (context `Composer`).

use gpui::actions;

actions!(
    composer,
    [
        /// Send the message (`cmd-enter`).
        Send,
        /// Close the window; asks whether to keep the draft if something changed
        /// (`escape`).
        Close,
        /// Save the draft now (`cmd-s`).
        SaveDraft,
        /// Attach files (`cmd-shift-a`).
        Attach,
        /// Show or hide the Cc and Bcc fields (`cmd-shift-c`).
        ToggleCcBcc,
        /// Move the draft from its tab to a window of its own.
        OpenInNewWindow,
    ]
);
