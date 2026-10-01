use crate::opcode::{Operation, RCIndex};

use super::{abi, ir::Snapshot};

const ALL: u8 = (1 << (abi::REFERENCE + 1)) - 1;
const NUMERIC: u8 = (1 << abi::INTEGER) | (1 << abi::NUMBER);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Registers([u64; 4]);

impl Registers {
    fn insert(&mut self, register: u8) {
        self.0[usize::from(register) / 64] |= 1 << (register % 64);
    }

    fn range(&mut self, start: usize, end: usize) {
        assert!(start <= end && end <= 256);
        for register in start..end {
            self.insert(register as u8);
        }
    }

    fn operand(&mut self, operand: RCIndex) {
        if let RCIndex::Register(register) = operand {
            self.insert(register.0);
        }
    }

    pub fn contains(self, register: u8) -> bool {
        self.0[usize::from(register) / 64] & (1 << (register % 64)) != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Helper {
    pub kind: u32,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

impl Helper {
    fn new(kind: u32, a: u8, b: u32, c: u32) -> Self {
        Self {
            kind,
            a: u32::from(a),
            b,
            c,
        }
    }

    fn operand(operand: RCIndex) -> u32 {
        match operand {
            RCIndex::Register(index) => u32::from(index.0),
            RCIndex::Constant(index) => abi::CONSTANT_OPERAND | u32::from(index.0),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Access {
    pub reads: Registers,
    pub writes: Registers,
    pub helper: Option<Helper>,
    tags: u8,
}

impl Access {
    pub fn new(op: Operation, snapshot: &Snapshot) -> Self {
        use Operation::*;
        let mut result = Self::default();
        match op {
            Move { dest, source } => {
                result.reads.insert(source.0);
                result.writes.insert(dest.0);
                result.tags = ALL;
                result.helper = Some(Helper::new(
                    abi::HELPER_MOVE,
                    dest.0,
                    u32::from(source.0),
                    0,
                ));
            }
            LoadConstant { dest, constant } => {
                result.writes.insert(dest.0);
                result.tags = 1 << snapshot.constants[usize::from(constant.0)].tag;
                result.helper = Some(Helper::new(
                    abi::HELPER_CONSTANT,
                    dest.0,
                    u32::from(constant.0),
                    0,
                ));
            }
            LoadBool { dest, .. } | Not { dest, .. } => {
                if let Not { source, .. } = op {
                    result.reads.insert(source.0);
                }
                result.writes.insert(dest.0);
                result.tags = 1 << abi::BOOLEAN;
            }
            LoadNil { dest, count } => {
                result.writes.range(
                    usize::from(dest.0),
                    usize::from(dest.0) + usize::from(count),
                );
                result.tags = 1 << abi::NIL;
            }
            NewTable {
                dest,
                array_size,
                map_size,
            } => {
                result.writes.insert(dest.0);
                result.tags = 1 << abi::REFERENCE;
                result.helper = Some(Helper::new(
                    abi::HELPER_NEW_TABLE,
                    dest.0,
                    u32::from(array_size),
                    u32::from(map_size),
                ));
            }
            GetTable { dest, table, key } => {
                result.reads.insert(table.0);
                result.reads.operand(key);
                result.writes.insert(dest.0);
                result.tags = ALL;
                result.helper = Some(Helper::new(
                    abi::HELPER_GET_TABLE,
                    dest.0,
                    u32::from(table.0),
                    Helper::operand(key),
                ));
            }
            SetTable { table, key, value } => {
                result.reads.insert(table.0);
                result.reads.operand(key);
                result.reads.operand(value);
                result.helper = Some(Helper::new(
                    abi::HELPER_SET_TABLE,
                    table.0,
                    Helper::operand(key),
                    Helper::operand(value),
                ));
            }
            GetUpTable { dest, table, key } => {
                result.reads.operand(key);
                result.writes.insert(dest.0);
                result.tags = ALL;
                result.helper = Some(Helper::new(
                    abi::HELPER_GET_UP_TABLE,
                    dest.0,
                    u32::from(table.0),
                    Helper::operand(key),
                ));
            }
            SetUpTable { table, key, value } => {
                result.reads.operand(key);
                result.reads.operand(value);
                result.helper = Some(Helper::new(
                    abi::HELPER_SET_UP_TABLE,
                    table.0,
                    Helper::operand(key),
                    Helper::operand(value),
                ));
            }
            GetUpValue { dest, source } => {
                result.writes.insert(dest.0);
                result.tags = ALL;
                result.helper = Some(Helper::new(
                    abi::HELPER_GET_UPVALUE,
                    dest.0,
                    u32::from(source.0),
                    0,
                ));
            }
            SetUpValue { dest, source } => {
                result.reads.insert(source.0);
                result.helper = Some(Helper::new(
                    abi::HELPER_SET_UPVALUE,
                    dest.0,
                    u32::from(source.0),
                    0,
                ));
            }
            Add { dest, left, right }
            | Sub { dest, left, right }
            | Mul { dest, left, right }
            | Div { dest, left, right } => {
                result.reads.operand(left);
                result.reads.operand(right);
                result.writes.insert(dest.0);
                result.tags = if matches!(op, Div { .. }) {
                    1 << abi::NUMBER
                } else {
                    NUMERIC
                };
            }
            Eq { left, right, .. } | Less { left, right, .. } | LessEq { left, right, .. } => {
                result.reads.operand(left);
                result.reads.operand(right);
            }
            NumericForPrep { base, .. } => {
                result.reads.insert(base.0);
                result.reads.insert(base.0 + 2);
                result.writes.insert(base.0);
                result.tags = NUMERIC;
            }
            NumericForLoop { base, .. } => {
                result
                    .reads
                    .range(usize::from(base.0), usize::from(base.0) + 3);
                result.writes.insert(base.0);
                result.writes.insert(base.0 + 3);
                result.tags = NUMERIC;
            }
            Test { value, .. } => result.reads.insert(value.0),
            Jump { close_upvalues, .. } if close_upvalues.is_none() => {}
            SetList { .. }
            | Call { .. }
            | TailCall { .. }
            | Return { .. }
            | VarArgs { .. }
            | MarkToBeClosed { .. }
            | Jump { .. }
            | TestSet { .. }
            | Closure { .. }
            | GenericForCall { .. }
            | GenericForLoop { .. }
            | Method { .. }
            | Concat { .. }
            | Length { .. }
            | Minus { .. }
            | IDiv { .. }
            | Mod { .. }
            | Pow { .. }
            | BitAnd { .. }
            | BitOr { .. }
            | BitXor { .. }
            | ShiftLeft { .. }
            | ShiftRight { .. }
            | BitNot { .. } => {
                result.reads.range(0, snapshot.registers);
                result.writes = result.reads;
                result.tags = ALL;
            }
        }
        result
    }

    pub fn permits_scalar_tag(self, tag: u64) -> bool {
        tag < abi::REFERENCE && self.tags & (1 << tag) != 0
    }

    pub fn permits_helper(self, kind: u32, a: u32, b: u32, c: u32) -> bool {
        self.helper == Some(Helper { kind, a, b, c })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        ConstantIndex16, ConstantIndex8, Opt254, RegisterIndex as R, UpValueIndex as U, VarCount,
    };

    fn snapshot() -> Snapshot {
        Snapshot {
            operations: super::super::resources::owned(&[]),
            constants: super::super::resources::owned(&[
                abi::Slot {
                    tag: abi::NIL,
                    bits: 0,
                },
                abi::Slot {
                    tag: abi::BOOLEAN,
                    bits: 1,
                },
                abi::Slot {
                    tag: abi::INTEGER,
                    bits: 42,
                },
                abi::Slot {
                    tag: abi::NUMBER,
                    bits: 0.5f64.to_bits(),
                },
                abi::Slot {
                    tag: abi::REFERENCE,
                    bits: 0,
                },
            ]),
            registers: 256,
            upvalues: 1,
            prototypes: 0,
        }
    }

    fn check(op: Operation, reads: &[u8], writes: &[u8], tags: &[u64], helper: Option<Helper>) {
        let access = Access::new(op, &snapshot());
        for register in 0..=255 {
            assert_eq!(
                access.reads.contains(register),
                reads.contains(&register),
                "read {register}: {op:?}"
            );
            assert_eq!(
                access.writes.contains(register),
                writes.contains(&register),
                "write {register}: {op:?}"
            );
        }
        for tag in [
            abi::NIL,
            abi::BOOLEAN,
            abi::INTEGER,
            abi::NUMBER,
            abi::REFERENCE,
            5,
            u64::MAX,
        ] {
            assert_eq!(
                access.permits_scalar_tag(tag),
                tags.contains(&tag),
                "tag {tag}: {op:?}"
            );
        }
        assert_eq!(access.helper, helper, "{op:?}");
        if let Some(helper) = helper {
            assert!(access.permits_helper(helper.kind, helper.a, helper.b, helper.c));
            for actual in [
                Helper {
                    kind: helper.kind + 1,
                    ..helper
                },
                Helper {
                    a: helper.a + 1,
                    ..helper
                },
                Helper {
                    b: helper.b ^ abi::CONSTANT_OPERAND,
                    ..helper
                },
                Helper {
                    c: helper.c ^ 1,
                    ..helper
                },
            ] {
                assert!(!access.permits_helper(actual.kind, actual.a, actual.b, actual.c));
            }
        }
    }

    #[test]
    fn register_bitmap_handles_word_boundaries_and_full_frame_without_aliasing() {
        let mut registers = Registers::default();
        for register in [0, 63, 64, 127, 128, 191, 192, 255] {
            registers.insert(register);
        }
        for register in 0..=255 {
            assert_eq!(
                registers.contains(register),
                [0, 63, 64, 127, 128, 191, 192, 255].contains(&register)
            );
        }
        registers.range(0, 256);
        assert!((0..=255).all(|register| registers.contains(register)));
        let mut empty = Registers::default();
        empty.range(256, 256);
        assert_eq!(empty, Registers::default());
    }

    #[test]
    fn scalar_moves_constants_boolean_and_nil_have_checked_outputs() {
        let scalars = &[abi::NIL, abi::BOOLEAN, abi::INTEGER, abi::NUMBER];
        check(
            Operation::Move {
                dest: R(255),
                source: R(64),
            },
            &[64],
            &[255],
            scalars,
            Some(Helper::new(abi::HELPER_MOVE, 255, 64, 0)),
        );
        for index in 0..5 {
            let tags: &[u64] = if index == 4 { &[] } else { &[index] };
            check(
                Operation::LoadConstant {
                    dest: R(192),
                    constant: ConstantIndex16(index as u16),
                },
                &[],
                &[192],
                tags,
                Some(Helper::new(abi::HELPER_CONSTANT, 192, index as u32, 0)),
            );
        }
        check(
            Operation::LoadBool {
                dest: R(0),
                value: true,
                skip_next: false,
            },
            &[],
            &[0],
            &[abi::BOOLEAN],
            None,
        );
        check(
            Operation::Not {
                dest: R(128),
                source: R(191),
            },
            &[191],
            &[128],
            &[abi::BOOLEAN],
            None,
        );
        check(
            Operation::LoadNil {
                dest: R(63),
                count: 3,
            },
            &[],
            &[63, 64, 65],
            &[abi::NIL],
            None,
        );
        check(
            Operation::LoadNil {
                dest: R(255),
                count: 0,
            },
            &[],
            &[],
            &[abi::NIL],
            None,
        );
    }

    #[test]
    fn every_heap_helper_has_exact_operand_encoding_and_no_upvalue_register_alias() {
        let scalars = &[abi::NIL, abi::BOOLEAN, abi::INTEGER, abi::NUMBER];
        let key = ConstantIndex8(4).into();
        let encoded = abi::CONSTANT_OPERAND | 4;
        check(
            Operation::NewTable {
                dest: R(7),
                array_size: 3,
                map_size: 2,
            },
            &[],
            &[7],
            &[],
            Some(Helper::new(abi::HELPER_NEW_TABLE, 7, 3, 2)),
        );
        check(
            Operation::GetTable {
                dest: R(7),
                table: R(6),
                key,
            },
            &[6],
            &[7],
            scalars,
            Some(Helper::new(abi::HELPER_GET_TABLE, 7, 6, encoded)),
        );
        check(
            Operation::SetTable {
                table: R(6),
                key,
                value: R(63).into(),
            },
            &[6, 63],
            &[],
            &[],
            Some(Helper::new(abi::HELPER_SET_TABLE, 6, encoded, 63)),
        );
        check(
            Operation::GetUpTable {
                dest: R(7),
                table: U(0),
                key: R(192).into(),
            },
            &[192],
            &[7],
            scalars,
            Some(Helper::new(abi::HELPER_GET_UP_TABLE, 7, 0, 192)),
        );
        check(
            Operation::SetUpTable {
                table: U(0),
                key,
                value: R(255).into(),
            },
            &[255],
            &[],
            &[],
            Some(Helper::new(abi::HELPER_SET_UP_TABLE, 0, encoded, 255)),
        );
        check(
            Operation::GetUpValue {
                dest: R(7),
                source: U(0),
            },
            &[],
            &[7],
            scalars,
            Some(Helper::new(abi::HELPER_GET_UPVALUE, 7, 0, 0)),
        );
        check(
            Operation::SetUpValue {
                dest: U(0),
                source: R(7),
            },
            &[7],
            &[],
            &[],
            Some(Helper::new(abi::HELPER_SET_UPVALUE, 0, 7, 0)),
        );
    }

    #[test]
    fn guarded_numeric_and_loop_paths_describe_may_writes_not_must_definitions() {
        let numeric = &[abi::INTEGER, abi::NUMBER];
        macro_rules! arithmetic {
            ($op:ident, $tags:expr) => {
                check(
                    Operation::$op {
                        dest: R(0),
                        left: R(0).into(),
                        right: ConstantIndex8(2).into(),
                    },
                    &[0],
                    &[0],
                    $tags,
                    None,
                );
            };
        }
        arithmetic!(Add, numeric);
        arithmetic!(Sub, numeric);
        arithmetic!(Mul, numeric);
        arithmetic!(Div, &[abi::NUMBER]);
        for op in [
            Operation::Eq {
                left: R(0).into(),
                right: R(1).into(),
                skip_if: true,
            },
            Operation::Less {
                left: R(0).into(),
                right: R(1).into(),
                skip_if: false,
            },
            Operation::LessEq {
                left: R(0).into(),
                right: R(1).into(),
                skip_if: true,
            },
        ] {
            check(op, &[0, 1], &[], &[], None);
        }
        check(
            Operation::NumericForPrep {
                base: R(252),
                jump: 0,
            },
            &[252, 254],
            &[252],
            numeric,
            None,
        );
        check(
            Operation::NumericForLoop {
                base: R(252),
                jump: -1,
            },
            &[252, 253, 254],
            &[252, 255],
            numeric,
            None,
        );
        check(
            Operation::Test {
                value: R(255),
                is_true: false,
            },
            &[255],
            &[],
            &[],
            None,
        );
        check(
            Operation::Jump {
                offset: 0,
                close_upvalues: Opt254::none(),
            },
            &[],
            &[],
            &[],
            None,
        );
    }

    #[test]
    fn interpreter_barriers_conservatively_cover_the_declared_frame() {
        let source = snapshot();
        for op in [
            Operation::Call {
                func: R(0),
                args: VarCount::variable(),
                returns: VarCount::variable(),
            },
            Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            },
            Operation::Jump {
                offset: 0,
                close_upvalues: Opt254::some(0),
            },
            Operation::TestSet {
                dest: R(1),
                value: R(0),
                is_true: false,
            },
        ] {
            let access = Access::new(op, &source);
            assert!((0..=255).all(
                |register| access.reads.contains(register) && access.writes.contains(register)
            ));
            assert!(access.helper.is_none());
        }
        let mut source = snapshot();
        source.registers = 4;
        let access = Access::new(
            Operation::Return {
                start: R(0),
                count: VarCount::constant(0),
            },
            &source,
        );
        assert!((0..4).all(|register| access.reads.contains(register)));
        assert!((4..=255).all(|register| !access.reads.contains(register)));
    }
}
