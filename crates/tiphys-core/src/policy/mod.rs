//! What an action is allowed to be.
//!
//! Every action a tool plans is given a [`Verdict`] before anything runs: a
//! [`Class`], and for anything out of the ordinary, why. The class decides
//! whether the action runs, asks the owner first, or is refused.
//!
//! The rules are written for an Ubuntu server that exists for the agent. The
//! machine is the real boundary; these rules keep the owner informed of what
//! reaches beyond the agent's own home, and hold a short list of things that
//! are never done.

use serde::{Deserialize, Serialize};

pub mod paths;
pub mod shell;

/// How much an action changes, and so who has to agree to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// Reads. Runs without asking.
    Observe,
    /// Changes made as the Tiphys user, inside its own home. Runs without
    /// asking unless the owner has said to ask.
    Change,
    /// Reaches outside the agent's own home, needs root, or touches a
    /// secret. Asks first.
    System,
    /// Refused, whoever asks.
    Never,
}

/// What the rules say about one action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub class: Class,
    /// Why it asks or is refused, for the owner. Empty when it simply runs.
    pub why: String,
}

impl Verdict {
    pub fn observe() -> Self {
        Self {
            class: Class::Observe,
            why: String::new(),
        }
    }

    pub fn change() -> Self {
        Self {
            class: Class::Change,
            why: String::new(),
        }
    }

    pub fn system(why: impl Into<String>) -> Self {
        Self {
            class: Class::System,
            why: why.into(),
        }
    }

    pub fn never(why: impl Into<String>) -> Self {
        Self {
            class: Class::Never,
            why: why.into(),
        }
    }

    /// The stricter of two verdicts, for an action made of several parts.
    pub fn and(self, other: Self) -> Self {
        if other.class > self.class {
            other
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_order_from_harmless_to_refused() {
        assert!(Class::Observe < Class::Change);
        assert!(Class::Change < Class::System);
        assert!(Class::System < Class::Never);
    }

    #[test]
    fn the_stricter_verdict_wins_and_keeps_its_reason() {
        let asked = Verdict::system("outside home");
        assert_eq!(Verdict::change().and(asked.clone()), asked);
        assert_eq!(asked.clone().and(Verdict::observe()), asked);
        let refused = Verdict::never("the key store");
        assert_eq!(asked.and(refused.clone()), refused);
        // Of two equally strict, the first is kept.
        assert_eq!(
            Verdict::system("first").and(Verdict::system("second")).why,
            "first"
        );
    }
}
