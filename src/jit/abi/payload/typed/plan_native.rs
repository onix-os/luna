use cranelift_codegen::ir::{
    condcodes::IntCC, types, AbiParam, InstBuilder, MemFlagsData, Value as IrValue,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Module};

use super::*;
use crate::Value;

#[path = "plan_ir.rs"]
mod ir;

type Entry = for<'gc> unsafe extern "C" fn(
    *const *mut i64,
    *const *mut Value<'gc>,
    super::super::Write,
    u32,
) -> u32;

struct Code<'p> {
    module: Option<JITModule>,
    plan: &'p Plan,
    entry: Option<Entry>,
}

impl Drop for Code<'_> {
    fn drop(&mut self) {
        unsafe { self.module.take().unwrap().free_memory() };
    }
}

fn pointer(
    builder: &mut FunctionBuilder<'_>,
    table: IrValue,
    cache: &mut [Option<IrValue>; 256],
    index: usize,
) -> IrValue {
    *cache[index].get_or_insert_with(|| {
        builder
            .ins()
            .load(types::I64, MemFlagsData::new(), table, index as i32 * 8)
    })
}

impl<'p> Code<'p> {
    fn new(source: &Snapshot, plan: &'p Plan) -> Result<Self, CompileError> {
        Self::inspect(source, plan, |_, _| {})
    }

    fn inspect(
        source: &Snapshot,
        plan: &'p Plan,
        inspect: impl FnOnce(&cranelift_codegen::ir::Function, &dyn cranelift_codegen::isa::TargetIsa),
    ) -> Result<Self, CompileError> {
        plan.verify(source)?;
        let mut code = Self {
            module: Some(JITModule::new(
                JITBuilder::new(default_libcall_names()).unwrap(),
            )),
            plan,
            entry: None,
        };
        let module = code.module.as_mut().unwrap();
        let mut context = module.make_context();
        context
            .func
            .signature
            .params
            .extend([types::I64, types::I64, types::I64, types::I32].map(AbiParam::new));
        context
            .func
            .signature
            .returns
            .push(AbiParam::new(types::I32));
        let id = module
            .declare_anonymous_function(&context.func.signature)
            .unwrap();
        let mut writer = module.make_signature();
        writer.params.extend([AbiParam::new(types::I64); 3]);
        let mut frontend = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut context.func, &mut frontend);
            let writer = builder.import_signature(writer);
            let entry = builder.create_block();
            let done = builder.create_block();
            builder.append_block_param(done, types::I32);
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            let [pointers, destinations, callee, budget]: [_; 4] =
                builder.block_params(entry).try_into().unwrap();
            let mut fields = [None; 256];
            let mut cells = [None; 256];
            let mut inputs = [None; 256];
            let mut results = [None; MAX_STEPS];
            for (index, step) in plan.steps.iter().enumerate() {
                let body = builder.create_block();
                let count = builder.ins().iconst(types::I32, index as i64);
                let exhausted = builder
                    .ins()
                    .icmp(IntCC::UnsignedLessThanOrEqual, budget, count);
                builder
                    .ins()
                    .brif(exhausted, done, &[count.into()], body, &[]);
                builder.switch_to_block(body);
                let mut input = |value: Input| match value {
                    Input::Binding(index) => *inputs[usize::from(index)].get_or_insert_with(|| {
                        let field =
                            pointer(&mut builder, pointers, &mut fields, usize::from(index));
                        builder
                            .ins()
                            .load(types::I64, MemFlagsData::new(), field, 0)
                    }),
                    Input::Previous(index) => results[usize::from(index)].unwrap(),
                    Input::Literal(value) => builder.ins().iconst(types::I64, value),
                };
                let result = match step.expression {
                    Expression::Copy(a) => input(a),
                    Expression::Negate(a) => {
                        let a = input(a);
                        builder.ins().ineg(a)
                    }
                    Expression::Add(a, b) | Expression::Sub(a, b) | Expression::Mul(a, b) => {
                        let a = input(a);
                        let b = input(b);
                        match step.expression {
                            Expression::Add(..) => builder.ins().iadd(a, b),
                            Expression::Sub(..) => builder.ins().isub(a, b),
                            Expression::Mul(..) => builder.ins().imul(a, b),
                            _ => unreachable!(),
                        }
                    }
                };
                match step.store {
                    Store::Typed(index) => {
                        let field =
                            pointer(&mut builder, pointers, &mut fields, usize::from(index));
                        builder.ins().store(MemFlagsData::new(), result, field, 0);
                    }
                    Store::Canonical => {
                        let destination = pointer(
                            &mut builder,
                            destinations,
                            &mut cells,
                            usize::from(step.destination),
                        );
                        let null = builder.ins().iconst(types::I64, 0);
                        builder
                            .ins()
                            .call_indirect(writer, callee, &[destination, null, result]);
                    }
                }
                results[index] = Some(result);
            }
            let count = builder.ins().iconst(types::I32, plan.steps.len() as i64);
            builder.ins().jump(done, &[count.into()]);
            builder.switch_to_block(done);
            let count = builder.block_params(done)[0];
            builder.ins().return_(&[count]);
            builder.seal_all_blocks();
            builder.finalize(module.isa().frontend_config());
        }
        ir::verify(source, plan, &context.func, module.isa())?;
        inspect(&context.func, module.isa());
        module.define_function(id, &mut context).unwrap();
        module.finalize_definitions().unwrap();
        code.entry = Some(unsafe { std::mem::transmute(module.get_finalized_function(id)) });
        Ok(code)
    }

    fn run(&self, values: &mut [Value<'_>], budget: u32) -> Option<u32> {
        tests::with_bindings(self.plan, values, |pointers, destinations| unsafe {
            self.entry.unwrap()(
                pointers.as_ptr(),
                destinations.as_ptr(),
                super::super::terminal_write,
                budget,
            )
        })
    }
}

#[test]
fn native_source_regions_match_every_canonical_prefix() {
    crate::Lua::empty().enter(|ctx| {
        let closure = crate::Closure::load(
            ctx,
            None,
            b"local a,b=... local x=a+b local y=x*a x=-x y=y-x return y",
        )
        .unwrap();
        let compiled = Snapshot::new(&closure.prototype(), 4096, 2 * 1024 * 1024).unwrap();
        let start = compiled
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Add { .. }))
            .unwrap();
        let end = compiled
            .operations
            .iter()
            .position(|op| matches!(op, Operation::Return { .. }))
            .unwrap();
        for (source, start, end) in [(tests::sample(), 0, 6), (compiled, start, end)] {
            for start in start..end {
                let plan = Plan::new(&source, start, end).unwrap();
                let code = Code::new(&source, &plan).unwrap();
                for a in [i64::MIN, -1, 0, 1, i64::MAX] {
                    for b in [i64::MIN, -1, 0, 1, i64::MAX] {
                        for budget in (0..=plan.steps.len() as u32 + 1).chain([u32::MAX]) {
                            let mut actual = [Value::Table(crate::Table::new(&ctx)); 256];
                            for (index, &register) in plan.bindings.iter().enumerate() {
                                actual[usize::from(register)] =
                                    Value::Integer(if index % 2 == 0 { a } else { b });
                            }
                            let mut expected = actual;
                            let count = budget.min(plan.steps.len() as u32);
                            assert_eq!(
                                code.run(&mut actual[..source.registers], budget),
                                Some(count)
                            );
                            tests::interpret(
                                &source,
                                start,
                                count as usize,
                                &mut expected[..source.registers],
                            );
                            for (a, b) in actual.into_iter().zip(expected) {
                                super::super::assert_same(a, b);
                            }
                        }
                    }
                }
            }
        }
    });
}

#[test]
fn native_source_regions_preserve_admission_and_capacity_boundaries() {
    use crate::types::RegisterIndex;

    let source = tests::snapshot(
        &[Operation::Add {
            dest: RegisterIndex(255),
            left: RegisterIndex(255).into(),
            right: RegisterIndex(0).into(),
        }; MAX_STEPS],
        256,
    );
    let plan = Plan::new(&source, 0, MAX_STEPS).unwrap();
    let code = Code::new(&source, &plan).unwrap();
    assert_eq!(
        unsafe {
            code.entry.unwrap()(
                std::ptr::null(),
                std::ptr::null(),
                super::super::terminal_write,
                0,
            )
        },
        0
    );
    for budget in [0, 1, 63, 64, 65, u32::MAX] {
        let mut actual = [Value::Nil; 256];
        actual[0] = Value::Integer(i64::MAX);
        actual[255] = Value::Integer(i64::MIN);
        let mut expected = actual;
        let count = budget.min(MAX_STEPS as u32);
        assert_eq!(code.run(&mut actual, budget), Some(count));
        tests::interpret(&source, 0, count as usize, &mut expected);
        for (a, b) in actual.into_iter().zip(expected) {
            super::super::assert_same(a, b);
        }
    }
    for register in [0, 255] {
        for value in [Value::Nil, Value::Boolean(true), Value::Number(-0.0)] {
            let mut values = [Value::Integer(7); 256];
            values[register] = value;
            let before = values;
            assert_eq!(code.run(&mut values, 64), None);
            for (a, b) in values.into_iter().zip(before) {
                super::super::assert_same(a, b);
            }
        }
    }
    drop(code);
    let mut plan = plan;
    plan.steps[0].store = Store::Canonical;
    assert!(Code::new(&source, &plan).is_err());
}
