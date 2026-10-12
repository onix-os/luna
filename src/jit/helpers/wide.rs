use super::*;

pub(in crate::jit) const fn operands(packed: u32, pc: u32) -> u64 {
    (packed as u64) | ((pc as u64) << 32)
}

pub(in crate::jit) unsafe extern "C" fn call(
    host: *mut abi::Host,
    slots: *mut Slot,
    operands: u64,
    hash: u64,
) -> u32 {
    unsafe {
        call_selected::<{ abi::HELPER_GET_UP_TABLE }, true>(
            host,
            slots,
            operands as u32,
            hash as u32,
            (hash >> 32) as u32,
            (operands >> 32) as u32,
        )
    }
}

helper_entry!(@body direct, [extern "C"], [],
    [host: *mut abi::Host, slots: *mut Slot, operands: u64, hash: u64],
    [let a = operands as u32; let b = hash as u32; let c = (hash >> 32) as u32; let pc = (operands >> 32) as u32;],
    [host, slots, a, b, c, pc], abi::HELPER_GET_UP_TABLE, true);

#[test]
fn wide_operands_preserve_reserved_bits_and_full_pc() {
    let words = [
        0,
        1,
        255,
        256,
        65535,
        65536,
        0x00ff_ffff,
        0x0100_0000,
        u32::MAX,
    ];
    for packed in words {
        for pc in words {
            let wide = operands(packed, pc);
            assert_eq!(wide as u32, packed);
            assert_eq!((wide >> 32) as u32, pc);
            assert_eq!(
                unsafe { call(std::ptr::null_mut(), std::ptr::null_mut(), wide, u64::MAX) },
                abi::HELPER_DECLINED
            );
        }
    }
}

#[cfg(all(
    not(miri),
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
mod native;
