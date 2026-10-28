use super::*;

thread_local! {
    static REQUESTED: RefCell<Option<Vec<(String, String)>>> = const { RefCell::new(None) };
}

pub(in crate::jit) fn target_features() -> Result<String, Error> {
    let module =
        JITModule::new(native_builder(cranelift_native::builder()).map_err(|_| Error::Identity)?);
    let flags = module.isa().isa_flags();
    let features = flags
        .iter()
        .map(|value| format!("{}={}", value.name, value.value_string()))
        .collect::<Vec<_>>()
        .join(";");
    if features.len() > 32768 {
        return Err(Error::Limit);
    }
    Ok(features)
}

pub(in crate::jit) fn with_target<T>(features: &str, run: impl FnOnce() -> T) -> Result<T, Error> {
    if features.len() > 32768 {
        return Err(Error::Limit);
    }
    let module =
        JITModule::new(native_builder(cranelift_native::builder()).map_err(|_| Error::Identity)?);
    let expected = module.isa().isa_flags();
    let fields: Vec<_> = features.split(';').collect();
    if fields.len() != expected.len() || fields.len() > 128 {
        return Err(Error::Identity);
    }
    let mut probe = cranelift_native::builder().map_err(|_| Error::Identity)?;
    let mut request = Vec::new();
    request
        .try_reserve_exact(fields.len())
        .map_err(|_| Error::Allocation)?;
    for (field, descriptor) in fields.iter().zip(&expected) {
        let (name, value) = field.split_once('=').ok_or(Error::Identity)?;
        if name != descriptor.name || value.is_empty() || value.len() > 128 {
            return Err(Error::Identity);
        }
        let canonical = match descriptor.kind() {
            settings::SettingKind::Bool => matches!(value, "true" | "false"),
            settings::SettingKind::Num => value
                .parse::<u8>()
                .ok()
                .is_some_and(|number| number.to_string() == value),
            settings::SettingKind::Enum => value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            settings::SettingKind::Preset => false,
        };
        if !canonical {
            return Err(Error::Identity);
        }
        probe.set(name, value).map_err(|_| Error::Identity)?;
        request.push((
            owned_symbol(name).map_err(|_| Error::Allocation)?,
            owned_symbol(value).map_err(|_| Error::Allocation)?,
        ));
    }
    struct Restore(Option<Vec<(String, String)>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            REQUESTED.replace(self.0.take());
        }
    }
    let restore = Restore(REQUESTED.replace(Some(request)));
    let result = run();
    drop(restore);
    Ok(result)
}

pub(in crate::jit) fn apply_target(
    builder: &mut cranelift_codegen::isa::Builder,
) -> Result<(), JitError> {
    REQUESTED.with_borrow(|features| {
        if let Some(features) = features {
            for (name, value) in features {
                builder
                    .set(name, value)
                    .map_err(|error| JitError::Compilation(error.to_string()))?;
            }
        }
        Ok(())
    })
}
