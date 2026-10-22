//! Serde helpers that write `mazemail_types` ids in their public form (`a_b`, `f_c`)
//! instead of the raw integers their own `Serialize` impls use.

use std::collections::BTreeSet;

use mazemail_types::PublicId;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serializer};

fn parse<'de, T: PublicId, D: Deserializer<'de>>(text: &str) -> Result<T, D::Error> {
    T::parse_public(text).map_err(|error| D::Error::custom(format!("{text:?}: {error}")))
}

/// `T` as a public id string.
pub(crate) mod one {
    use super::*;

    pub(crate) fn serialize<T: PublicId, S: Serializer>(
        id: &T,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&id.to_public())
    }

    pub(crate) fn deserialize<'de, T: PublicId, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        let text = String::deserialize(deserializer)?;
        parse::<T, D>(&text)
    }
}

/// `Option<T>` as a public id string or `null`.
pub(crate) mod option {
    use super::*;

    pub(crate) fn serialize<T: PublicId, S: Serializer>(
        id: &Option<T>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match id {
            Some(id) => serializer.serialize_some(&id.to_public()),
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, T: PublicId, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<T>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|text| parse::<T, D>(&text))
            .transpose()
    }
}

/// `Vec<T>` as an array of public id strings.
pub(crate) mod vec {
    use super::*;

    pub(crate) fn serialize<T: PublicId, S: Serializer>(
        ids: &[T],
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(ids.iter().map(|id| id.to_public()))
    }

    pub(crate) fn deserialize<'de, T: PublicId, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Vec<T>, D::Error> {
        Vec::<String>::deserialize(deserializer)?
            .iter()
            .map(|text| parse::<T, D>(text))
            .collect()
    }
}

/// `Option<BTreeSet<T>>` as an array of public id strings or `null`.
pub(crate) mod option_set {
    use super::*;

    pub(crate) fn serialize<T: PublicId, S: Serializer>(
        ids: &Option<BTreeSet<T>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match ids {
            Some(ids) => {
                let ids: Vec<String> = ids.iter().map(|id| id.to_public()).collect();
                serializer.serialize_some(&ids)
            }
            None => serializer.serialize_none(),
        }
    }

    pub(crate) fn deserialize<'de, T: PublicId + Ord, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BTreeSet<T>>, D::Error> {
        Option::<Vec<String>>::deserialize(deserializer)?
            .map(|ids| ids.iter().map(|text| parse::<T, D>(text)).collect())
            .transpose()
    }
}
