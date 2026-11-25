use super::abi;
use crate::Value;

pub(super) enum Registers<'a, 'gc> {
    Scalar(&'a mut [abi::Slot]),
    Rooted(abi::roots::call::preserved::Preserved<'a, 'gc>),
}

impl<'gc> Registers<'_, 'gc> {
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Scalar(slots) => slots.len(),
            Self::Rooted(file) => file.len(),
        }
    }

    pub(super) fn snapshot(&mut self) -> Option<abi::roots::call::Snapshot<'_, 'gc>> {
        match self {
            Self::Scalar(_) => None,
            Self::Rooted(file) => Some(file.snapshot()),
        }
    }

    pub(super) fn materialize(&self, values: &mut [Value<'gc>]) -> bool {
        match self {
            Self::Rooted(file) => file.materialize(values),
            Self::Scalar(slots) => {
                for (slot, value) in slots.iter().zip(values) {
                    slot.write_back(value);
                }
                true
            }
        }
    }

    pub(super) fn recapture(&mut self, values: &[Value<'gc>]) -> bool {
        match self {
            Self::Rooted(file) => file.recapture(values),
            Self::Scalar(slots) => {
                for (slot, value) in slots.iter_mut().zip(values.iter().copied()) {
                    *slot = abi::Slot::from_value(value);
                }
                true
            }
        }
    }
}
