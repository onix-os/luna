#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(in crate::jit) enum ProjectionMode {
    Canonical,
    #[cfg(test)]
    Projected,
}

impl ProjectionMode {
    pub(in crate::jit) const fn enabled(self) -> bool {
        match self {
            Self::Canonical => false,
            #[cfg(test)]
            Self::Projected => true,
        }
    }

    pub(super) const fn with_helpers(self, count: usize) -> Self {
        if count == 0 {
            Self::Canonical
        } else {
            self
        }
    }
}

const _: () = {
    assert!(std::mem::size_of::<ProjectionMode>() == std::mem::size_of::<bool>());
    assert!(std::mem::align_of::<ProjectionMode>() == std::mem::align_of::<bool>());
};

#[cfg(test)]
mod tests {
    use super::ProjectionMode;

    #[test]
    fn mode_storage_preserves_boolean_field_size_and_alignment() {
        assert_eq!(std::mem::size_of::<ProjectionMode>(), 1);
        assert_eq!(std::mem::align_of::<ProjectionMode>(), 1);
    }

    #[test]
    fn helper_selection_preserves_the_existing_boolean_truth_table() {
        for mode in [ProjectionMode::Canonical, ProjectionMode::Projected] {
            for count in [0, 1, 2, 64, 256, usize::MAX] {
                assert_eq!(
                    mode.with_helpers(count).enabled(),
                    mode.enabled() && count != 0
                );
            }
        }
    }

    #[test]
    fn canonical_mode_never_enables_projection() {
        for count in 0..=256 {
            assert_eq!(
                ProjectionMode::Canonical.with_helpers(count),
                ProjectionMode::Canonical
            );
            assert!(!ProjectionMode::Canonical.with_helpers(count).enabled());
        }
    }
}
