pub(super) fn try_array<T, E, const N: usize>(
    mut value: impl FnMut(usize) -> Result<T, E>,
) -> Result<[T; N], E> {
    let mut values = std::array::from_fn(|_| None);
    for (index, slot) in values.iter_mut().enumerate() {
        *slot = Some(value(index)?);
    }
    Ok(values.map(|value| value.expect("missing fixed frontend value")))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        panic::{catch_unwind, AssertUnwindSafe},
        rc::Rc,
    };

    use super::*;

    struct Probe(usize, Rc<Cell<u16>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            let bit = 1 << self.0;
            assert_eq!(self.1.get() & bit, 0);
            self.1.set(self.1.get() | bit);
        }
    }

    #[test]
    fn nine_values_preserve_declaration_order_and_single_ownership() {
        let dropped = Rc::new(Cell::new(0));
        let mut expected = 0;
        let values = try_array::<_, (), 9>(|index| {
            assert_eq!(index, expected);
            expected += 1;
            Ok(Probe(index, dropped.clone()))
        })
        .unwrap();
        assert_eq!(expected, 9);
        assert_eq!(
            std::array::from_fn::<_, 9, _>(|index| values[index].0),
            [0, 1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(dropped.get(), 0);
        drop(values);
        assert_eq!(dropped.get(), 511);
    }

    #[test]
    fn every_declaration_error_stops_immediately_and_drops_only_the_prefix() {
        for failure in 0..9 {
            let dropped = Rc::new(Cell::new(0));
            let calls = Cell::new(0);
            let result = try_array::<_, usize, 9>(|index| {
                calls.set(calls.get() + 1);
                if index == failure {
                    Err(index)
                } else {
                    Ok(Probe(index, dropped.clone()))
                }
            });
            assert_eq!(result.err(), Some(failure));
            assert_eq!(calls.get(), failure + 1);
            assert_eq!(dropped.get(), (1 << failure) - 1);
        }
    }

    #[test]
    fn callback_panic_drops_the_initialized_prefix_without_later_calls() {
        for failure in 0..9 {
            let dropped = Rc::new(Cell::new(0));
            let calls = Cell::new(0);
            assert!(catch_unwind(AssertUnwindSafe(|| {
                let _ = try_array::<_, (), 9>(|index| {
                    calls.set(calls.get() + 1);
                    assert_ne!(index, failure, "injected declaration panic");
                    Ok(Probe(index, dropped.clone()))
                });
            }))
            .is_err());
            assert_eq!(calls.get(), failure + 1);
            assert_eq!(dropped.get(), (1 << failure) - 1);
        }
    }

    #[test]
    fn empty_array_never_calls_the_fallible_callback() {
        let values = try_array::<usize, (), 0>(|_| panic!("empty array callback")).unwrap();
        assert_eq!(values, []);
    }

    #[test]
    fn fixed_array_keeps_borrowed_values_and_alignment() {
        #[repr(align(256))]
        struct Aligned<'a>(&'a str);
        let source = String::from("fixed");
        let values = try_array::<_, (), 2>(|_| Ok(Aligned(&source))).unwrap();
        assert_eq!(values[1].0, "fixed");
        assert_eq!((&values as *const _ as usize) % 256, 0);
    }
}
