use std::fmt;

use serde::{Deserialize, Serialize};

/// Wraps data that must never appear in logs: message content, subjects, addresses.
///
/// `Debug` and `Display` print `<redacted>`. Use [`Sensitive::expose`] to get the value.
#[derive(Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sensitive<T>(T);

impl<T> Sensitive<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn expose(&self) -> &T {
        &self.0
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Sensitive<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

impl<T> fmt::Debug for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl<T> fmt::Display for Sensitive<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_prints_the_value() {
        let subject = Sensitive::new("Quarterly numbers".to_owned());
        assert_eq!(format!("{subject:?} {subject}"), "<redacted> <redacted>");
        assert_eq!(subject.expose(), "Quarterly numbers");
        assert_eq!(
            serde_json::to_string(&subject).unwrap(),
            "\"Quarterly numbers\""
        );
    }
}
