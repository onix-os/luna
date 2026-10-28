use super::*;
use crate::jit::{readonly_lookup as read, work};

#[cfg(test)]
mod tests;

thread_local! {
    static EMITTING: Cell<bool> = const { Cell::new(false) };
}

pub(in crate::jit::backend) fn assert_native_memory_allowed() {
    assert!(
        !EMITTING.get(),
        "emission attempted native allocation or protection"
    );
}

fn without_native_memory<T>(run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            EMITTING.set(self.0);
        }
    }
    let _restore = Restore(EMITTING.replace(true));
    run()
}

pub(in crate::jit) fn emit(
    source: &Snapshot,
    route: Route,
    resume: bool,
    limits: work::Limits,
    metadata: BudgetAllocator,
    output: BudgetAllocator,
) -> Result<Bundle, JitError> {
    let total = MappingCounter::new(metadata.0.clone());
    let (result, bundle) = without_native_memory(|| {
        capture(output, || {
            let bound = work::Expansion::admit(source, limits)?;
            let compile = |readonly, work| {
                compile_image_to(
                    source,
                    total.clone(),
                    0,
                    metadata.clone(),
                    work,
                    ImageOptions {
                        selection: Selection {
                            projected: false,
                            readonly_scope: readonly,
                            scoped_helpers: false,
                            #[cfg(test)]
                            leaf: false,
                            #[cfg(test)]
                            cell_kernel: false,
                            #[cfg(test)]
                            integer_activation: false,
                            #[cfg(test)]
                            failure: Failure::None,
                        },
                        rooted_moves: false,
                        destination: Destination::Emit,
                    },
                )?
                .emitted()
            };
            let ordinary_relocations = compile(false, limits)?;
            match route {
                Route::Ordinary => {}
                Route::Arrays => {
                    let _ = array::compile_to(
                        source,
                        total.clone(),
                        0,
                        metadata.clone(),
                        limits,
                        #[cfg(test)]
                        Failure::None,
                        Destination::Emit,
                    )
                    .and_then(Compilation::emitted);
                }
                Route::Readonly if read::Plan::recognize(source).is_some() => {
                    let remaining = work::Limits {
                        instructions: limits.instructions.saturating_sub(bound.instructions),
                        blocks: limits.blocks.saturating_sub(bound.blocks),
                        relocations: limits.relocations.saturating_sub(ordinary_relocations),
                    };
                    if let Ok(relocations) = compile(true, remaining) {
                        if resume
                            && read::binding::integer_resume::Plan::recognize(source).is_some()
                        {
                            if let Ok(bound) = work::Expansion::admit(source, remaining)
                                .and_then(|bound| readonly_lookup::expansion(bound, remaining))
                            {
                                let extra = work::Limits {
                                    instructions: remaining
                                        .instructions
                                        .saturating_sub(bound.instructions),
                                    blocks: remaining.blocks.saturating_sub(bound.blocks),
                                    relocations: remaining.relocations.saturating_sub(relocations),
                                };
                                let _ = integer_resume::compile_to(
                                    source,
                                    total.clone(),
                                    0,
                                    metadata.clone(),
                                    extra,
                                    Destination::Emit,
                                )
                                .and_then(Compilation::emitted);
                            }
                        }
                    }
                }
                Route::Readonly => {}
            }
            Ok(())
        })
    })
    .map_err(|error| match error {
        Error::Allocation | Error::Limit => JitError::ResourceLimit("compiler emission"),
        _ => JitError::Compilation("compiler emission artifact".into()),
    })?;
    result?;
    assert_eq!(total.load(Ordering::Relaxed), 0);
    assert_eq!(total.requested(), 0);
    Ok(bundle)
}
