//! The captains' rules for Soulseek (server settings, see `settings`):
//! who may use it at all, and who may share their library.
//!
//! Each rule has a server-wide switch by rank, and captains can override
//! it for single members (let one trusted pirate in, keep one captain
//! out). `Off` is absolute, overrides included: it is the switch for
//! "nobody, right now".
//!
//! Every Soulseek connection leaves from the machine Inkult runs on, so
//! what is shared is uploaded from the host's address: these rules let
//! whoever runs the server decide that, above each user's own choice.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Deserializer, Serialize};

use crate::auth::{Role, UserId};

/// Who may do something on Soulseek: use it at all, or share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SoulseekAccess {
    #[serde(rename = "all")]
    Everyone,
    #[serde(rename = "captains")]
    Captains,
    #[serde(rename = "off")]
    Off,
}

impl SoulseekAccess {
    pub fn allows(self, role: Role) -> bool {
        match self {
            Self::Everyone => true,
            Self::Captains => role.is_captain(),
            Self::Off => false,
        }
    }

    /// Like `Deserialize`, but also reads the yes/no of settings files
    /// written before captains-only sharing existed.
    pub fn deserialize_or_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Bool(bool),
            Access(SoulseekAccess),
        }
        Ok(match Stored::deserialize(d)? {
            Stored::Bool(true) => Self::Everyone,
            Stored::Bool(false) => Self::Off,
            Stored::Access(access) => access,
        })
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Everyone => 0,
            Self::Captains => 1,
            Self::Off => 2,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Everyone,
            1 => Self::Captains,
            _ => Self::Off,
        }
    }
}

/// A captain's choice for one member, over the server-wide switch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Override {
    /// Follow the switch.
    #[default]
    Auto,
    /// Yes, whatever their rank (unless the switch is off).
    Allowed,
    /// No, whatever their rank.
    Blocked,
}

impl Override {
    fn rank(self) -> u8 {
        match self {
            Self::Blocked => 0,
            Self::Auto => 1,
            Self::Allowed => 2,
        }
    }

    /// Whether switching to `other` can let the member do more, now or
    /// once a switch is widened.
    pub fn is_widened_by(self, other: Self) -> bool {
        other.rank() > self.rank()
    }

    fn decide(self, switch: SoulseekAccess, role: Role) -> bool {
        switch != SoulseekAccess::Off
            && match self {
                Self::Auto => switch.allows(role),
                Self::Allowed => true,
                Self::Blocked => false,
            }
    }
}

/// A member's overrides; members without an entry follow the switches.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRules {
    #[serde(default)]
    pub soulseek: Override,
    #[serde(default)]
    pub sharing: Override,
}

impl MemberRules {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Whether a member with these choices (of `role`) may use Soulseek
    /// under the switch `access`.
    pub fn may_use(self, access: SoulseekAccess, role: Role) -> bool {
        self.soulseek.decide(access, role)
    }

    /// Whether they may share under the switches `access` and `sharing`.
    /// Sharing needs Soulseek: whatever lets more people use it can let
    /// more share, so a check for "more sharing" must ask this, not
    /// compare the sharing settings alone.
    pub fn may_share(self, access: SoulseekAccess, sharing: SoulseekAccess, role: Role) -> bool {
        self.may_use(access, role) && self.sharing.decide(sharing, role)
    }
}

/// The rules in force, shared between the settings (which change them)
/// and every Soulseek session (which apply them as they run).
#[derive(Debug, Clone)]
pub struct SoulseekRules(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    access: AtomicU8,
    sharing: AtomicU8,
    members: Mutex<BTreeMap<UserId, MemberRules>>,
    /// The rank a captain last gave each member since startup. A rank is
    /// otherwise known from the request that started a session, which
    /// can be outdated by the time a slow sign-in finishes.
    ranks: Mutex<HashMap<UserId, Role>>,
}

impl SoulseekRules {
    pub fn new(
        access: SoulseekAccess,
        sharing: SoulseekAccess,
        members: BTreeMap<UserId, MemberRules>,
    ) -> Self {
        Self(Arc::new(Inner {
            access: AtomicU8::new(access.to_u8()),
            sharing: AtomicU8::new(sharing.to_u8()),
            members: Mutex::new(members),
            ranks: Mutex::new(HashMap::new()),
        }))
    }

    pub fn access(&self) -> SoulseekAccess {
        SoulseekAccess::from_u8(self.0.access.load(Ordering::Relaxed))
    }

    pub fn sharing(&self) -> SoulseekAccess {
        SoulseekAccess::from_u8(self.0.sharing.load(Ordering::Relaxed))
    }

    fn member(&self, user: UserId) -> MemberRules {
        crate::jobs::lock(&self.0.members)
            .get(&user)
            .copied()
            .unwrap_or_default()
    }

    /// A captain gave `user` the rank `role`: it wins over the rank any of
    /// their sessions started with (see [`Self::rank`]).
    pub fn set_rank(&self, user: UserId, role: Role) {
        crate::jobs::lock(&self.0.ranks).insert(user, role);
    }

    pub fn forget_rank(&self, user: UserId) {
        crate::jobs::lock(&self.0.ranks).remove(&user);
    }

    /// `user`'s rank: the one a captain last gave them, else `started_as`
    /// (the rank known when their session started, current unless changed
    /// since, and then recorded here).
    pub fn rank(&self, user: UserId, started_as: Role) -> Role {
        crate::jobs::lock(&self.0.ranks)
            .get(&user)
            .copied()
            .unwrap_or(started_as)
    }

    /// Whether `user` (of `role`) may use Soulseek.
    pub fn may_use(&self, user: UserId, role: Role) -> bool {
        self.member(user).may_use(self.access(), role)
    }

    /// Whether `user` may share their library (they still decide for
    /// themselves; nobody shares unless they turned it on).
    pub fn may_share(&self, user: UserId, role: Role) -> bool {
        self.member(user)
            .may_share(self.access(), self.sharing(), role)
    }

    pub fn set(
        &self,
        access: SoulseekAccess,
        sharing: SoulseekAccess,
        members: BTreeMap<UserId, MemberRules>,
    ) {
        self.0.access.store(access.to_u8(), Ordering::Relaxed);
        self.0.sharing.store(sharing.to_u8(), Ordering::Relaxed);
        *crate::jobs::lock(&self.0.members) = members;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_by_role() {
        for (access, captain, pirate) in [
            (SoulseekAccess::Everyone, true, true),
            (SoulseekAccess::Captains, true, false),
            (SoulseekAccess::Off, false, false),
        ] {
            assert_eq!(access.allows(Role::Captain), captain, "{access:?}");
            assert_eq!(access.allows(Role::Pirate), pirate, "{access:?}");
            assert_eq!(SoulseekAccess::from_u8(access.to_u8()), access);
        }
    }

    const ANNE: UserId = UserId(1);
    const JACK: UserId = UserId(2);

    #[test]
    fn changes_reach_every_clone() {
        use SoulseekAccess::*;
        let rules = SoulseekRules::new(Everyone, Off, BTreeMap::new());
        let seen_by_session = rules.clone();
        rules.set(Everyone, Captains, BTreeMap::new());
        assert!(seen_by_session.may_use(JACK, Role::Pirate));
        assert!(seen_by_session.may_share(ANNE, Role::Captain));
        assert!(!seen_by_session.may_share(JACK, Role::Pirate));
    }

    #[test]
    fn member_overrides_beat_the_switch_except_off() {
        use Override::*;
        use SoulseekAccess::*;
        let member = |soulseek, sharing| MemberRules { soulseek, sharing };
        let rules = SoulseekRules::new(Captains, Captains, BTreeMap::new());
        assert!(!rules.may_use(JACK, Role::Pirate));

        // A trusted pirate is let in, and may share.
        let members = BTreeMap::from([(JACK, member(Allowed, Allowed))]);
        rules.set(Captains, Captains, members.clone());
        assert!(rules.may_use(JACK, Role::Pirate));
        assert!(rules.may_share(JACK, Role::Pirate));
        // Off is off for everyone, overrides included.
        rules.set(Off, Captains, members.clone());
        assert!(!rules.may_use(JACK, Role::Pirate));
        rules.set(Captains, Off, members);
        assert!(rules.may_use(JACK, Role::Pirate));
        assert!(!rules.may_share(JACK, Role::Pirate));

        // A captain kept out, or kept from sharing.
        rules.set(
            Everyone,
            Everyone,
            BTreeMap::from([(ANNE, member(Blocked, Auto))]),
        );
        assert!(!rules.may_use(ANNE, Role::Captain));
        assert!(!rules.may_share(ANNE, Role::Captain), "no use, no sharing");
        rules.set(
            Everyone,
            Everyone,
            BTreeMap::from([(ANNE, member(Auto, Blocked))]),
        );
        assert!(rules.may_use(ANNE, Role::Captain));
        assert!(!rules.may_share(ANNE, Role::Captain));
        assert!(rules.may_share(JACK, Role::Pirate), "others unaffected");

        // A rank a captain gave wins over the one a session started with.
        assert_eq!(rules.rank(JACK, Role::Captain), Role::Captain);
        rules.set_rank(JACK, Role::Pirate);
        assert_eq!(rules.rank(JACK, Role::Captain), Role::Pirate);
        rules.forget_rank(JACK);
        assert_eq!(rules.rank(JACK, Role::Captain), Role::Captain);

        assert!(Blocked.is_widened_by(Auto));
        assert!(Auto.is_widened_by(Allowed));
        assert!(!Allowed.is_widened_by(Auto));
    }
}
