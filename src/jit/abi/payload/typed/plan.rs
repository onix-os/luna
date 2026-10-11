use allocator_api2::vec::Vec;

use crate::{
    jit::{abi, ir::Snapshot, resources::BudgetAllocator, JitError},
    opcode::{Operation, RCIndex},
};

const MAX_STEPS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Binding(u16),
    Previous(u16),
    Literal(i64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expression {
    Copy(Input),
    Negate(Input),
    Add(Input, Input),
    Sub(Input, Input),
    Mul(Input, Input),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    Typed(u16),
    Canonical,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Step {
    destination: u8,
    expression: Expression,
    store: Store,
}

struct Plan {
    start: u32,
    registers: usize,
    bindings: Vec<u8, BudgetAllocator>,
    steps: Vec<Step, BudgetAllocator>,
}

fn invalid() -> JitError {
    JitError::Compilation("invalid typed canonical region".into())
}

impl Plan {
    fn new(source: &Snapshot, start: usize, end: usize) -> Result<Self, JitError> {
        source.verify()?;
        if start >= end || end > source.operations.len() || end - start > MAX_STEPS {
            return Err(invalid());
        }
        let allocator = source.operations.allocator().clone();
        let mut plan = Self {
            start: start as u32,
            registers: source.registers,
            bindings: Vec::new_in(allocator.clone()),
            steps: Vec::new_in(allocator),
        };
        plan.bindings
            .try_reserve_exact(source.registers)
            .map_err(|_| JitError::ResourceLimit("typed region bindings"))?;
        plan.steps
            .try_reserve_exact(end - start)
            .map_err(|_| JitError::ResourceLimit("typed region steps"))?;
        let mut values = [None; 256];
        for operation in &source.operations[start..end] {
            let mut input = |operand: RCIndex| -> Result<Input, JitError> {
                match operand {
                    RCIndex::Register(register) => {
                        let index = usize::from(register.0);
                        if let Some(value) = values[index] {
                            return Ok(value);
                        }
                        let value = Input::Binding(plan.bindings.len() as u16);
                        plan.bindings.push(register.0);
                        values[index] = Some(value);
                        Ok(value)
                    }
                    RCIndex::Constant(index) => {
                        let slot = source.constants[usize::from(index.0)];
                        if slot.tag != abi::INTEGER {
                            return Err(invalid());
                        }
                        Ok(Input::Literal(slot.bits as i64))
                    }
                }
            };
            let (destination, expression) = match *operation {
                Operation::Add { dest, left, right } => {
                    (dest.0, Expression::Add(input(left)?, input(right)?))
                }
                Operation::Sub { dest, left, right } => {
                    (dest.0, Expression::Sub(input(left)?, input(right)?))
                }
                Operation::Mul { dest, left, right } => {
                    (dest.0, Expression::Mul(input(left)?, input(right)?))
                }
                Operation::Move { dest, source } => {
                    (dest.0, Expression::Copy(input(RCIndex::Register(source))?))
                }
                Operation::Minus { dest, source } => (
                    dest.0,
                    Expression::Negate(input(RCIndex::Register(source))?),
                ),
                Operation::LoadConstant { dest, constant } => {
                    let slot = source.constants[usize::from(constant.0)];
                    if slot.tag != abi::INTEGER {
                        return Err(invalid());
                    }
                    (dest.0, Expression::Copy(Input::Literal(slot.bits as i64)))
                }
                _ => return Err(invalid()),
            };
            values[usize::from(destination)] = Some(Input::Previous(plan.steps.len() as u16));
            plan.steps.push(Step {
                destination,
                expression,
                store: Store::Canonical,
            });
        }
        for step in &mut plan.steps {
            if let Some(index) = plan.bindings.iter().position(|&r| r == step.destination) {
                step.store = Store::Typed(index as u16);
            }
        }
        plan.verify(source)?;
        Ok(plan)
    }

    fn verify(&self, source: &Snapshot) -> Result<(), JitError> {
        source.verify()?;
        let start = self.start as usize;
        let end = start.checked_add(self.steps.len()).ok_or_else(invalid)?;
        if self.registers != source.registers
            || self.steps.is_empty()
            || self.steps.len() > MAX_STEPS
            || end > source.operations.len()
            || self.bindings.len() > source.registers
        {
            return Err(invalid());
        }
        let mut map = [None; 256];
        for (slot, &register) in self.bindings.iter().enumerate() {
            let register = usize::from(register);
            if register >= self.registers || map[register].replace(slot as u16).is_some() {
                return Err(invalid());
            }
        }
        let mut definitions = [None; 256];
        let mut used = [false; 256];
        for (position, (operation, step)) in source.operations[start..end]
            .iter()
            .zip(&self.steps)
            .enumerate()
        {
            let mut check = |operand: RCIndex, input: Input| -> bool {
                match operand {
                    RCIndex::Register(register) => {
                        let register = usize::from(register.0);
                        if let Some(previous) = definitions[register] {
                            input == Input::Previous(previous)
                        } else if let Some(binding) = map[register] {
                            used[usize::from(binding)] = true;
                            input == Input::Binding(binding)
                        } else {
                            false
                        }
                    }
                    RCIndex::Constant(index) => {
                        let constant = source.constants[usize::from(index.0)];
                        constant.tag == abi::INTEGER
                            && input == Input::Literal(constant.bits as i64)
                    }
                }
            };
            let (destination, valid) = match (*operation, step.expression) {
                (Operation::Add { dest, left, right }, Expression::Add(a, b))
                | (Operation::Sub { dest, left, right }, Expression::Sub(a, b))
                | (Operation::Mul { dest, left, right }, Expression::Mul(a, b)) => {
                    (dest.0, check(left, a) && check(right, b))
                }
                (Operation::Move { dest, source }, Expression::Copy(value))
                | (Operation::Minus { dest, source }, Expression::Negate(value)) => {
                    (dest.0, check(RCIndex::Register(source), value))
                }
                (Operation::LoadConstant { dest, constant }, Expression::Copy(value)) => {
                    let constant = source.constants[usize::from(constant.0)];
                    (
                        dest.0,
                        constant.tag == abi::INTEGER
                            && value == Input::Literal(constant.bits as i64),
                    )
                }
                _ => return Err(invalid()),
            };
            if !valid
                || step.destination != destination
                || step.store
                    != map[usize::from(destination)].map_or(Store::Canonical, Store::Typed)
            {
                return Err(invalid());
            }
            definitions[usize::from(destination)] = Some(position as u16);
        }
        if used[..self.bindings.len()].contains(&false) {
            return Err(invalid());
        }
        Ok(())
    }
}

#[path = "plan_tests.rs"]
mod tests;
