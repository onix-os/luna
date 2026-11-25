use std::cell::RefCell;

use cranelift_codegen::binemit::Reloc;
use cranelift_module::{DataId, FuncId, ModuleRelocTarget};

use super::*;

#[derive(Debug, PartialEq, Eq)]
pub(in crate::jit) struct Image {
    alignment: u64,
    bytes: Vec<u8>,
    pub(in crate::jit) relocations: Vec<Relocation>,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::jit) struct Relocation {
    offset: u32,
    kind: Reloc,
    target: String,
    addend: i64,
}

thread_local! {
    static CAPTURES: RefCell<Option<Vec<Image>>> = const { RefCell::new(None) };
}

pub(in crate::jit) fn capture<T>(run: impl FnOnce() -> T) -> (T, Vec<Image>) {
    struct Restore(Option<Vec<Image>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CAPTURES.replace(self.0.take());
        }
    }
    let restore = Restore(CAPTURES.replace(Some(Vec::new())));
    let result = run();
    let images = CAPTURES.replace(None).unwrap();
    drop(restore);
    (result, images)
}

pub(super) fn record(
    bytes: &[u8],
    alignment: u64,
    relocations: &[ModuleReloc],
    module: &JITModule,
) {
    CAPTURES.with_borrow_mut(|captures| {
        let Some(captures) = captures else { return };
        let relocations = relocations
            .iter()
            .map(|relocation| {
                let declaration = match relocation.name {
                    ModuleRelocTarget::User {
                        namespace: 0,
                        index,
                    } => format!(
                        "{:?}",
                        module
                            .declarations()
                            .get_function_decl(FuncId::from_u32(index))
                    ),
                    ModuleRelocTarget::User {
                        namespace: 1,
                        index,
                    } => format!(
                        "{:?}",
                        module.declarations().get_data_decl(DataId::from_u32(index))
                    ),
                    ModuleRelocTarget::FunctionOffset(id, _) => {
                        format!("{:?}", module.declarations().get_function_decl(id))
                    }
                    _ => String::new(),
                };
                Relocation {
                    offset: relocation.offset,
                    kind: relocation.kind,
                    target: format!("{} {declaration}", relocation.name),
                    addend: relocation.addend,
                }
            })
            .collect();
        captures.push(Image {
            alignment,
            bytes: bytes.to_vec(),
            relocations,
        });
    });
}

#[test]
fn capture_restores_nested_and_panicking_scopes() {
    assert!(CAPTURES.with_borrow(Option::is_none));
    let (_, images) = capture(|| {
        CAPTURES.with_borrow_mut(|value| {
            value.as_mut().unwrap().push(Image {
                alignment: 16,
                bytes: vec![71],
                relocations: Vec::new(),
            })
        });
        let (_, nested) = capture(|| {});
        assert!(nested.is_empty());
        assert_eq!(
            CAPTURES.with_borrow(|value| value.as_ref().unwrap().len()),
            1
        );
        assert!(std::panic::catch_unwind(|| capture(|| std::panic::panic_any(23_u64))).is_err());
        assert_eq!(
            CAPTURES.with_borrow(|value| value.as_ref().unwrap().len()),
            1
        );
    });
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].bytes, [71]);
    assert!(CAPTURES.with_borrow(Option::is_none));
}
