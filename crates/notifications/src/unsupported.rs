//! Platforms without a notification backend.

use crate::{AppInfo, Error, Notice, Response};

pub(crate) async fn check(_app: &AppInfo) -> Result<&'static str, Error> {
    Err(Error::Unavailable(
        "not supported on this platform".to_owned(),
    ))
}

pub(crate) async fn prepare(_app: &AppInfo) -> Result<(), Error> {
    Err(Error::Unavailable(
        "not supported on this platform".to_owned(),
    ))
}

pub(crate) async fn show(_app: &AppInfo, _notice: Notice) -> Result<Response, Error> {
    Err(Error::Unavailable(
        "not supported on this platform".to_owned(),
    ))
}
