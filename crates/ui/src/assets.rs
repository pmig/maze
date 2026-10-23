//! Embedded assets: gpui-kit's component icons plus the extra Lucide icons Maze uses.
//!
//! Only the icons listed in [`MazeIcons`] are embedded (a few KiB), not the whole
//! Lucide catalog. Lucide icons are ISC licensed (Feather-derived ones MIT).

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};
use gpui_kit::assets::Assets as KitAssets;

pub use gpui_kit::assets::IconName;

gpui_kit::assets::icon_assets!(
    MazeIcons,
    [
        Archive,
        ArrowDownUp,
        ArrowUpRight,
        BookUser,
        CircleDot,
        Clock,
        CloudCheck,
        Columns3,
        FilePen,
        Flag,
        Forward,
        Layers,
        ListFilter,
        ListTree,
        Mail,
        MailOpen,
        Mailbox,
        Mails,
        PanelLeft,
        PanelRight,
        Paperclip,
        Puzzle,
        Reply,
        ReplyAll,
        Rows3,
        Send,
        Server,
        ShieldAlert,
        ShieldCheck,
        Sparkles,
        SquarePen,
        Table,
        Tag,
        Trash,
        Users,
        Wifi,
        WifiOff
    ]
);

/// The application's asset source. Pass it to `Application::with_assets`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = MazeIcons.load(path)? {
            return Ok(Some(bytes));
        }
        KitAssets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = KitAssets.list(path)?;
        paths.extend(MazeIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_maze_and_kit_icons() {
        for icon in [IconName::Archive, IconName::ReplyAll, IconName::Inbox] {
            let bytes = Assets.load(&icon.path()).unwrap();
            assert!(bytes.is_some(), "{icon:?} is not embedded");
        }
        let listed = Assets.list("icons/").unwrap();
        assert!(listed.iter().any(|p| p.as_ref() == "icons/paperclip.svg"));
    }
}
