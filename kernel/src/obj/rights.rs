use core::ops::BitAnd;

use super::ObjError;

/// The universal five rights (§3.3). Monotone-decreasing under attunement.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rights(u32);

impl Rights {
    pub const QUERY: Rights = Rights(1 << 0);
    pub const INVOKE: Rights = Rights(1 << 1);
    pub const TRAVERSE: Rights = Rights(1 << 2);
    pub const MINT: Rights = Rights(1 << 3);
    pub const REVOKE: Rights = Rights(1 << 4);

    pub const fn empty() -> Self {
        Rights(0)
    }

    /// Union-OR two right-masks (compose a multi-right set at bootstrap; §5.4).
    pub const fn or(self, other: Rights) -> Self {
        Rights(self.0 | other.0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(&self, r: Rights) -> bool {
        self.0 & r.0 == r.0
    }

    /// Monotone attunement (§7.2.2): keep only the bits already held that the
    /// requested mask keeps. The `NoAmplification` error is unreachable via
    /// AND (a result can never gain a bit the source lacked); it is a canary
    /// for future bugs.
    pub fn attune(&self, keep: Rights) -> Result<Rights, ObjError> {
        let r = self.0 & keep.0;
        if r & !self.0 == 0 {
            Ok(Rights(r))
        } else {
            Err(ObjError::NoAmplification)
        }
    }
}

impl BitAnd for Rights {
    type Output = Rights;
    fn bitand(self, rhs: Rights) -> Rights {
        Rights(self.0 & rhs.0)
    }
}

/// The orthogonal contract-right dimension (READ/WRITE, ...). §3.3.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ContractRights(u32);

impl ContractRights {
    pub const fn empty() -> Self {
        ContractRights(0)
    }

    pub const fn bits(self) -> u32 {
        self.0
    }

    pub fn contains(&self, r: ContractRights) -> bool {
        self.0 & r.0 == r.0
    }
}

impl core::ops::BitAnd for ContractRights {
    type Output = ContractRights;
    fn bitand(self, rhs: ContractRights) -> ContractRights {
        ContractRights(self.0 & rhs.0)
    }
}

/// A capability's full rights: the universal five plus a contract dimension.
/// Both dimensions must shrink together monotonically (§7.2.2).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct CapRights {
    pub uni: Rights,
    pub contract: ContractRights,
}

impl CapRights {
    pub const fn new(uni: Rights, contract: ContractRights) -> Self {
        CapRights { uni, contract }
    }

    /// Dual monotone attunement across both dimensions (§7.2.2).
    pub fn attune(
        &self,
        keep: Rights,
        keep_contract: ContractRights,
    ) -> Result<CapRights, ObjError> {
        let u = self.uni & keep;
        let c = self.contract & keep_contract;
        if (u.0 & !self.uni.0) == 0 && (c.0 & !self.contract.0) == 0 {
            Ok(CapRights { uni: u, contract: c })
        } else {
            Err(ObjError::NoAmplification)
        }
    }
}