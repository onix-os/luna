use allocator_api2::vec::Vec;

use crate::{
    opcode::{Operation, RCIndex},
    table::PreparedStringHash,
    Constant, FunctionPrototype,
};

use super::{ir::Snapshot, JitError};

pub(super) struct Keys(Vec<u64, super::resources::BudgetAllocator>);

#[cfg(test)]
mod tests;

impl Keys {
    pub fn new(snapshot: &Snapshot, prototype: &FunctionPrototype<'_>) -> Option<Self> {
        let mut found = false;
        for &operation in &snapshot.operations {
            if let Operation::GetUpTable { key, .. } = operation {
                let RCIndex::Constant(index) = key else {
                    return None;
                };
                if !matches!(
                    prototype.constants.get(usize::from(index.0)),
                    Some(Constant::String(_))
                ) {
                    return None;
                }
                found = true;
            }
        }
        if !found {
            return None;
        }
        let mut hashes = Vec::new_in(snapshot.operations.allocator().clone());
        hashes.try_reserve_exact(prototype.constants.len()).ok()?;
        hashes.extend(prototype.constants.iter().map(|constant| match constant {
            Constant::String(key) => PreparedStringHash::new(*key).bits(),
            _ => 0,
        }));
        Some(Self(hashes))
    }

    pub fn hash(&self, index: u8) -> u64 {
        self.0[usize::from(index)]
    }

    pub fn verify(&self, snapshot: &Snapshot) -> Result<(), JitError> {
        if self.0.len() != snapshot.constants.len() {
            return Err(JitError::Compilation("invalid prepared key count".into()));
        }
        for &operation in &snapshot.operations {
            if let Operation::GetUpTable { key, .. } = operation {
                let RCIndex::Constant(index) = key else {
                    return Err(JitError::Compilation("invalid prepared key operand".into()));
                };
                if snapshot
                    .constants
                    .get(usize::from(index.0))
                    .is_none_or(|slot| slot.tag != super::abi::REFERENCE || slot.bits != 0)
                {
                    return Err(JitError::Compilation(
                        "invalid prepared key descriptor".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
