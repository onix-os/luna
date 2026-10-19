use crate::{fuel::count_fuel, Fuel};

use super::thread::LuaFrame;

pub(crate) struct NativeList<'a> {
    fuel: &'a mut Fuel,
    variable: bool,
}

impl<'a> NativeList<'a> {
    pub(crate) fn new(fuel: &'a mut Fuel, variable: bool) -> Self {
        Self { fuel, variable }
    }

    pub(crate) fn reborrow(&mut self) -> NativeList<'_> {
        NativeList::new(self.fuel, self.variable)
    }

    pub(crate) fn is_fixed(&self) -> bool {
        !self.variable
    }

    pub(crate) fn charge(&mut self, count: usize) {
        self.fuel.consume(LuaFrame::FUEL_PER_CALL);
        self.fuel
            .consume(count_fuel(LuaFrame::FUEL_PER_ITEM, count));
    }

    #[cfg(test)]
    pub(crate) fn charge_if_fixed(&mut self, count: usize) -> bool {
        if !self.is_fixed() {
            return false;
        }
        self.charge(count);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_list_charges_match_canonical_rules_at_every_width() {
        for variable in [false, true] {
            for initial in [i32::MIN, -1, 0, 1, 100, i32::MAX] {
                for count in [0, 1, 7, 256, i32::MAX as usize, usize::MAX] {
                    let mut actual = Fuel::with(initial);
                    let mut expected = Fuel::with(initial);
                    let accepted = NativeList::new(&mut actual, variable).charge_if_fixed(count);
                    assert_eq!(accepted, !variable);
                    if !variable {
                        expected.consume(LuaFrame::FUEL_PER_CALL);
                        expected.consume(count_fuel(LuaFrame::FUEL_PER_ITEM, count));
                    }
                    assert_eq!(actual.remaining(), expected.remaining());
                    assert_eq!(actual.is_interrupted(), expected.is_interrupted());
                }
            }
        }
    }

    #[test]
    fn native_fuel_is_disjoint_from_live_register_value_loans() {
        crate::Lua::empty().enter(|ctx| {
            let mut fuel = Fuel::with(100);
            let mut pc = 3;
            let mut values = [crate::Value::Integer(7)];
            super::super::thread::LuaRegisters::with_test_frame_state(
                ctx,
                &mut pc,
                &mut values,
                &mut fuel,
                false,
                |registers, mut list| {
                    let value = &mut registers.stack_frame[0];
                    assert!(list.reborrow().charge_if_fixed(3));
                    *value = crate::Value::Integer(19);
                    *registers.pc += 1;
                },
            );
            assert_eq!(fuel.remaining(), 93);
            assert_eq!(pc, 4);
            assert!(matches!(values[0], crate::Value::Integer(19)));
        });
    }
}
