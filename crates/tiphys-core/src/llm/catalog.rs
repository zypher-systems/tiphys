//! A connection's model list, kept on disk: `models/<connection>.json`.
//!
//! The list says which models a connection offers and what they cost. It is
//! fetched when a connection is set up or its models are browsed, and kept,
//! so that a run started later knows its prices without asking again. A list
//! that was never fetched just means prices are unknown until it is.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::Model;
use crate::config::valid_name;
use crate::files::{SHARED_DIR, SHARED_FILE, ensure_dir, write_atomic};
use crate::{Error, Result};

/// The directory under the state directory that holds the lists.
pub const MODELS_DIR: &str = "models";

/// A model list and when it was fetched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Catalog {
    pub fetched: DateTime<Utc>,
    pub models: Vec<Model>,
}

/// Keeps `models` as the list for `connection`.
pub fn store(home: &Path, connection: &str, models: &[Model]) -> Result<()> {
    let path = path(home, connection)?;
    ensure_dir(&home.join(MODELS_DIR), SHARED_DIR)?;
    let catalog = Catalog {
        fetched: Utc::now(),
        models: models.to_vec(),
    };
    let json = serde_json::to_vec_pretty(&catalog)
        .map_err(|e| Error::Io(format!("cannot encode the model list: {e}")))?;
    write_atomic(&path, &json, SHARED_FILE)
}

/// The kept list for `connection`, if there is a readable one. A damaged file
/// is treated as no list: the next fetch replaces it.
pub fn load(home: &Path, connection: &str) -> Option<Catalog> {
    let text = std::fs::read_to_string(path(home, connection).ok()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn path(home: &Path, connection: &str) -> Result<PathBuf> {
    valid_name(connection)?;
    Ok(home.join(MODELS_DIR).join(format!("{connection}.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spend::Rates;

    #[test]
    fn a_kept_list_comes_back_and_a_damaged_one_is_no_list() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(load(home.path(), "work"), None);

        let models = vec![Model {
            id: "vendor/model".into(),
            context: Some(200_000),
            rates: Some(Rates {
                input: 3.0,
                output: 15.0,
                cache_read: Some(0.3),
                cache_write: None,
            }),
            tools: Some(true),
        }];
        store(home.path(), "work", &models).unwrap();
        assert_eq!(load(home.path(), "work").unwrap().models, models);

        std::fs::write(home.path().join("models/work.json"), "{").unwrap();
        assert_eq!(load(home.path(), "work"), None);
        assert!(store(home.path(), "../work", &models).is_err());
    }
}
