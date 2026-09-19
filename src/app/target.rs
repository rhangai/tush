use std::str::FromStr;

use crate::util::str::SmallStr;
use anyhow::{Result, bail};

/// What tells one kind of target from another.
///
/// Reserved: keys and group names may not contain it, which building the
/// session refuses at the door — see
/// [`ReservedCharacter`](crate::error::AppConfigError::ReservedCharacter).
/// Otherwise `group:web` means one thing or another depending on what the
/// file happens to declare.
pub const TARGET_SEPARATOR: char = ':';

/// The prefix that means a group rather than a proc.
const GROUP: &str = "group";

/// Something to start, named the way a command line names it.
///
/// A bare name is a proc; `group:web` is every proc declared under `web`. The
/// prefix is on the group and not on the proc because the proc is the common
/// case and the common case should be the short one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// One proc, by the key it was declared under.
    Unit(SmallStr),
    /// Every proc in a group, by the group's name.
    Group(SmallStr),
}

impl FromStr for Target {
    type Err = anyhow::Error;

    /// Read one target off a command line.
    ///
    /// An unknown prefix is refused rather than read as a proc name: those
    /// cannot contain a colon, so a colon is always a prefix and a prefix
    /// nobody knows is a typo worth naming.
    fn from_str(target: &str) -> Result<Self> {
        let Some((kind, name)) = target.split_once(TARGET_SEPARATOR) else {
            if target.is_empty() {
                bail!("an empty target names nothing");
            }
            return Ok(Self::Unit(target.into()));
        };
        if kind != GROUP {
            bail!(
                "`{kind}{TARGET_SEPARATOR}` is not a kind of target; did you mean `{GROUP}{TARGET_SEPARATOR}{name}`?"
            );
        }
        if name.is_empty() {
            bail!("`{GROUP}{TARGET_SEPARATOR}` with no group after it names nothing");
        }
        Ok(Self::Group(name.into()))
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_bare_name_is_a_proc_and_a_prefixed_one_is_a_group() {
        assert_eq!(
            "server".parse::<Target>().unwrap(),
            Target::Unit("server".into())
        );
        assert_eq!(
            "group:web".parse::<Target>().unwrap(),
            Target::Group("web".into())
        );
    }

    /// A proc cannot have a colon in its name, so a colon is always a prefix
    /// — and a prefix nobody knows is a typo worth naming.
    #[test]
    fn an_unknown_prefix_is_refused_rather_than_read_as_a_name() {
        let error = "grupo:web".parse::<Target>().unwrap_err().to_string();
        assert!(error.contains("grupo:"), "{error}");
        assert!(error.contains("group:web"), "{error}");
    }

    #[test]
    fn a_target_has_to_name_something() {
        assert!("".parse::<Target>().is_err());
        assert!("group:".parse::<Target>().is_err());
    }
}
