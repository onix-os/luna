use super::*;
use crate::jit::{
    abi,
    resources::{self, Ledger},
};

fn limits() -> Limits {
    Limits {
        bytes: 1 << 20,
        operations: 65536,
        constants: 65536,
    }
}
fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(2 << 20))
}
fn snapshot() -> Snapshot {
    let mut operations = all_operations();
    for op in &mut operations {
        if let Operation::GenericForCall { var_count, .. } = op {
            *var_count = 1;
        }
    }
    operations.push(Operation::Return {
        start: RegisterIndex(0),
        count: VarCount::constant(0),
    });
    Snapshot {
        prepared_keys: None,
        operations: resources::owned(&operations),
        constants: resources::owned(&[Slot {
            tag: abi::NIL,
            bits: 0,
        }]),
        registers: 256,
        upvalues: 1,
        prototypes: 1,
    }
}
fn compare(a: &Snapshot, b: &Snapshot) {
    assert_eq!(
        (a.registers, a.upvalues, a.prototypes),
        (b.registers, b.upvalues, b.prototypes)
    );
    assert_eq!(&*a.operations, &*b.operations);
    assert_eq!(a.constants.len(), b.constants.len());
    for (a, b) in a.constants.iter().zip(&b.constants) {
        assert_eq!((a.tag, a.bits), (b.tag, b.bits));
    }
    assert_eq!(
        a.prepared_keys.as_ref().map(Keys::words),
        b.prepared_keys.as_ref().map(Keys::words)
    );
}

#[test]
fn every_operation_has_one_canonical_typed_record() {
    assert_eq!(all_operations().len(), 46);
    for op in all_operations() {
        let bytes = write_operation(op);
        assert_eq!(read_operation(&bytes).unwrap(), op);
        let mut pad = bytes;
        pad[7] = 1;
        assert_eq!(read_operation(&pad).err(), Some(Error::Padding));
    }
    for word in 0..=u16::MAX {
        let signed = <i16 as Field>::from_word(word).unwrap();
        assert_eq!(signed.word(), word);
        assert_eq!(
            <RegisterIndex as Field>::from_word(word).is_some(),
            word <= 255
        );
        assert_eq!(<RCIndex as Field>::from_word(word).is_some(), word <= 511);
        assert_eq!(<bool as Field>::from_word(word).is_some(), word <= 1);
        assert_eq!(<VarCount as Field>::from_word(word).is_some(), word <= 255);
        assert_eq!(<Opt254 as Field>::from_word(word).is_some(), word <= 255);
    }
    for op in [
        Operation::Jump {
            offset: i16::MIN,
            close_upvalues: Opt254::none(),
        },
        Operation::Jump {
            offset: i16::MAX,
            close_upvalues: Opt254::some(254),
        },
        Operation::LoadConstant {
            dest: RegisterIndex(255),
            constant: ConstantIndex16(u16::MAX),
        },
        Operation::Add {
            dest: RegisterIndex(255),
            left: RCIndex::Register(RegisterIndex(255)),
            right: RCIndex::Constant(ConstantIndex8(255)),
        },
        Operation::Call {
            func: RegisterIndex(0),
            args: VarCount::variable(),
            returns: VarCount::constant(254),
        },
    ] {
        assert_eq!(read_operation(&write_operation(op)).unwrap(), op);
    }
    let mut unknown = [0; OP_BYTES];
    assert_eq!(read_operation(&unknown).err(), Some(Error::Opcode));
    unknown[0] = 255;
    assert_eq!(read_operation(&unknown).err(), Some(Error::Opcode));
}

#[test]
fn full_owned_snapshots_roundtrip_and_survive_sender_release() {
    let mut source = snapshot();
    source.verify().unwrap();
    for bits in [
        0,
        1,
        u64::MAX,
        1 << 63,
        0x7ff0_0000_0000_0001,
        0x7ff8_0000_0000_1234,
    ] {
        source.constants.push(Slot {
            tag: abi::NUMBER,
            bits,
        });
    }
    source.constants.push(Slot {
        tag: abi::INTEGER,
        bits: i64::MIN as u64,
    });
    source.constants.push(Slot {
        tag: abi::BOOLEAN,
        bits: 1,
    });
    let encoded = encode(&source, limits(), allocator()).unwrap();
    assert_eq!(&encoded[..8], MAGIC);
    let output = allocator();
    let ledger = output.0.clone();
    let decoded = decode(&encoded, limits(), output).unwrap();
    compare(&source, &decoded);
    drop(source);
    drop(encoded);
    decoded.verify().unwrap();
    let replay = encode(&decoded, limits(), allocator()).unwrap();
    let another = decode(&replay, limits(), allocator()).unwrap();
    compare(&decoded, &another);
    assert!(ledger.current() > 0);
    drop(decoded);
    assert_eq!(ledger.current(), 0);
}

#[test]
fn prepared_parent_process_hash_words_are_preserved_without_gc_objects() {
    let mut source = Snapshot {
        prepared_keys: None,
        operations: resources::owned(&[
            Operation::GetUpTable {
                dest: RegisterIndex(0),
                table: UpValueIndex(0),
                key: RCIndex::Constant(ConstantIndex8(1)),
            },
            Operation::Return {
                start: RegisterIndex(0),
                count: VarCount::constant(1),
            },
        ]),
        constants: resources::owned(&[
            Slot {
                tag: abi::NUMBER,
                bits: (-0.0f64).to_bits(),
            },
            Slot {
                tag: abi::REFERENCE,
                bits: 0,
            },
        ]),
        registers: 1,
        upvalues: 1,
        prototypes: 0,
    };
    let hashes = resources::owned(&[0, u64::MAX]);
    source.prepared_keys = Some(Keys::from_words(hashes, &source).unwrap());
    source.verify().unwrap();
    let encoded = encode(&source, limits(), allocator()).unwrap();
    let decoded = decode(&encoded, limits(), allocator()).unwrap();
    compare(&source, &decoded);
    assert_eq!(
        decoded.prepared_keys.as_ref().unwrap().words(),
        [0, u64::MAX]
    );
    assert_eq!(
        &*encode(&decoded, limits(), allocator()).unwrap(),
        &*encoded
    );
}

#[test]
fn every_truncation_and_single_byte_mutation_is_total_and_canonical() {
    let source = snapshot();
    let encoded = encode(&source, limits(), allocator()).unwrap();
    let output = allocator();
    let ledger = output.0.clone();
    for len in 0..encoded.len() {
        assert!(decode(&encoded[..len], limits(), output.clone()).is_err());
        assert_eq!(ledger.current(), 0);
    }
    for index in 0..encoded.len() {
        for value in [0, 1, 127, 255] {
            let mut changed = encoded.to_vec();
            changed[index] = value;
            if let Ok(decoded) = decode(&changed, limits(), output.clone()) {
                decoded.verify().unwrap();
                assert_eq!(
                    &*encode(&decoded, limits(), allocator()).unwrap(),
                    &*changed
                );
            }
            assert_eq!(ledger.current(), 0);
        }
    }
    let mut trailing = encoded.to_vec();
    trailing.push(0);
    assert!(decode(&trailing, limits(), output).is_err());
}

#[test]
fn huge_counts_padding_and_invalid_reference_payloads_refuse_without_leaks() {
    let encoded = encode(&snapshot(), limits(), allocator()).unwrap();
    let output = allocator();
    let ledger = output.0.clone();
    for offset in [16, 24, 48, 56, 64] {
        let mut changed = encoded.to_vec();
        changed[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode(&changed, limits(), output.clone()).is_err());
        assert_eq!(ledger.current(), 0);
    }
    for offset in [12, HEADER + 7, HEADER + 4] {
        let mut changed = encoded.to_vec();
        changed[offset] = 255;
        assert!(decode(&changed, limits(), output.clone()).is_err());
        assert_eq!(ledger.current(), 0);
    }
    let mut invalid = encoded.to_vec();
    let constant = HEADER + snapshot().operations.len() * OP_BYTES;
    invalid[constant..constant + 8].copy_from_slice(&abi::REFERENCE.to_le_bytes());
    invalid[constant + 8..constant + 16].copy_from_slice(&1u64.to_le_bytes());
    assert_eq!(
        decode(&invalid, limits(), output).err(),
        Some(Error::Snapshot)
    );
    assert_eq!(ledger.current(), 0);
}

#[test]
fn byte_count_and_allocator_limits_are_exact_and_preserve_refusal() {
    let source = snapshot();
    let encoded = encode(&source, limits(), allocator()).unwrap();
    let exact = Limits {
        bytes: encoded.len(),
        ..limits()
    };
    assert!(encode(&source, exact, allocator()).is_ok());
    for small in [
        Limits {
            bytes: encoded.len() - 1,
            ..limits()
        },
        Limits {
            operations: 45,
            ..limits()
        },
        Limits {
            constants: 0,
            ..limits()
        },
    ] {
        assert_eq!(
            encode(&source, small, allocator()).err(),
            Some(Error::Limit)
        );
        assert!(decode(&encoded, small, allocator()).is_err());
    }
    let output = BudgetAllocator(Ledger::new(encoded.len() - 1));
    assert_eq!(
        encode(&source, exact, output.clone()).err(),
        Some(Error::Allocation)
    );
    assert_eq!(output.0.current(), 0);
    let mut failures = 0;
    for allowance in 0..4 {
        let output = allocator();
        output.0.fail_after(allowance);
        match decode(&encoded, limits(), output.clone()) {
            Ok(decoded) => drop(decoded),
            Err(error) => {
                assert_eq!(error, Error::Allocation);
                failures += 1;
                let mut probe = Vec::<u8, _>::new_in(output.clone());
                assert!(probe.try_reserve_exact(1).is_err());
            }
        }
        assert_eq!(output.0.current(), 0);
    }
    assert!(failures >= 2);
}

#[test]
fn receiver_owns_all_data_after_thread_handoff_and_sender_release() {
    fn send<T: Send>() {}
    send::<Snapshot>();
    send::<Vec<u8, BudgetAllocator>>();
    let source = snapshot();
    let sender = allocator();
    let sender_ledger = sender.0.clone();
    let packet = encode(&source, limits(), sender).unwrap();
    drop(source);
    let receiver = allocator();
    let receiver_ledger = receiver.0.clone();
    let decoded = std::thread::spawn(move || {
        let snapshot = decode(&packet, limits(), receiver).unwrap();
        drop(packet);
        snapshot
    })
    .join()
    .unwrap();
    assert_eq!(sender_ledger.current(), 0);
    assert!(receiver_ledger.current() > 0);
    decoded.verify().unwrap();
    drop(decoded);
    assert_eq!(receiver_ledger.current(), 0);
}

#[test]
fn real_workload_packets_survive_arena_destruction_without_gc_handles() {
    mod workloads {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/examples/jit_support/workloads.rs"
        ));
    }
    let packets = {
        let mut lua = crate::Lua::empty();
        lua.enter(|ctx| {
            let sources = workloads::WORKLOADS
                .iter()
                .map(|workload| workload.source)
                .chain([workloads::PREDICATE_SOURCE, workloads::COLD_SOURCE]);
            sources
                .map(|script| {
                    let closure = crate::Closure::load(ctx, None, script).unwrap();
                    let snapshot = Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
                    encode(&snapshot, limits(), allocator()).unwrap()
                })
                .collect::<std::vec::Vec<_>>()
        })
    };
    assert_eq!(packets.len(), 9);
    for packet in packets {
        let decoded = decode(&packet, limits(), allocator()).unwrap();
        decoded.verify().unwrap();
        assert_eq!(&*encode(&decoded, limits(), allocator()).unwrap(), &*packet);
    }
}
