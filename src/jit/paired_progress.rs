#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Counts(u64);

impl Counts {
    fn new(dispatches: u32, progress: u32) -> Self {
        Self((u64::from(dispatches) << 32) | u64::from(progress))
    }

    fn parts(self) -> (u32, u32) {
        ((self.0 >> 32) as u32, self.0 as u32)
    }

    fn add_pair(self, amount: u32) -> Option<Self> {
        let (dispatches, progress) = self.parts();
        dispatches.checked_add(amount)?;
        progress.checked_add(amount)?;
        Some(Self(self.0 + u64::from(amount) * 0x1_0000_0001))
    }

    fn add_native(self, amount: u32) -> Option<Self> {
        self.parts().1.checked_add(amount)?;
        Some(Self(self.0 + u64::from(amount)))
    }

    fn add_dispatch(self) -> Option<Self> {
        self.parts().0.checked_add(1)?;
        Some(Self(self.0 + (1 << 32)))
    }

    fn add_native_dispatch(self, amount: u32) -> Option<Self> {
        self.parts().0.checked_add(1)?;
        self.parts().1.checked_add(amount)?;
        Some(Self(self.0 + (1 << 32) + u64::from(amount)))
    }

    fn rollback_dispatch(self) -> Option<Self> {
        self.parts().0.checked_sub(1)?;
        Some(Self(self.0 - (1 << 32)))
    }
}

#[derive(Clone, Copy, Debug)]
enum Event {
    Native(u32),
    Mock(u32),
    Complete,
    Terminal,
    Error,
    Panic,
    Handoff,
    Atomic,
    AtomicError,
    AtomicPanic,
    Hook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    Budget,
    Exhausted,
    Terminal,
    Error,
    Panic,
    Handoff,
    Atomic,
    Hook,
    InvalidNative,
}

#[derive(Debug, PartialEq, Eq)]
struct Observation {
    dispatches: u32,
    reported: Option<u32>,
    returned: Option<u32>,
    end: End,
}

fn observation(dispatches: u32, progress: u32, native: u32, end: End) -> Observation {
    let success = !matches!(end, End::Error | End::Panic | End::InvalidNative);
    Observation {
        dispatches,
        reported: success.then(|| progress.checked_sub(native).unwrap()),
        returned: success.then_some(progress),
        end,
    }
}

fn separate(prefix: u32, budget: u32, events: &[Event]) -> Observation {
    if budget == 0 {
        assert_eq!(prefix, 0);
        return Observation {
            dispatches: 0,
            reported: None,
            returned: Some(0),
            end: End::Budget,
        };
    }
    let (mut dispatches, mut progress, mut native) =
        (0u128, u128::from(prefix), u128::from(prefix));
    let mut end = End::Exhausted;
    for event in events {
        if progress >= u128::from(budget) {
            end = End::Budget;
            break;
        }
        match *event {
            Event::Native(amount) | Event::Mock(amount) => {
                if u128::from(amount) > u128::from(budget) - progress {
                    end = End::InvalidNative;
                    break;
                }
                progress += u128::from(amount);
                if matches!(event, Event::Native(_)) {
                    native += u128::from(amount);
                } else {
                    dispatches += u128::from(amount);
                }
            }
            Event::Hook => {
                end = End::Hook;
                break;
            }
            Event::Atomic | Event::AtomicError | Event::AtomicPanic => {
                dispatches += 1;
                end = match event {
                    Event::Atomic => End::Atomic,
                    Event::AtomicError => End::Error,
                    Event::AtomicPanic => End::Panic,
                    _ => unreachable!(),
                };
                break;
            }
            _ => {
                dispatches += 1;
                match event {
                    Event::Complete => progress += 1,
                    Event::Terminal => end = End::Terminal,
                    Event::Error => end = End::Error,
                    Event::Panic => end = End::Panic,
                    Event::Handoff => {
                        dispatches -= 1;
                        end = End::Handoff;
                    }
                    _ => unreachable!(),
                }
                if !matches!(event, Event::Complete) {
                    break;
                }
            }
        }
        assert!(dispatches <= u128::from(u32::MAX));
        assert!(progress <= u128::from(budget));
        if progress >= u128::from(budget) {
            end = End::Budget;
            break;
        }
    }
    observation(
        u32::try_from(dispatches).unwrap(),
        u32::try_from(progress).unwrap(),
        u32::try_from(native).unwrap(),
        end,
    )
}

fn paired(prefix: u32, budget: u32, events: &[Event]) -> Observation {
    if budget == 0 {
        assert_eq!(prefix, 0);
        return Observation {
            dispatches: 0,
            reported: None,
            returned: Some(0),
            end: End::Budget,
        };
    }
    let mut counts = Counts::new(0, prefix);
    let mut native = prefix;
    let mut end = End::Exhausted;
    let mut exclude_terminal = false;
    for event in events {
        let (dispatches, progress) = counts.parts();
        assert!(dispatches <= progress);
        if progress >= budget {
            end = End::Budget;
            break;
        }
        match *event {
            Event::Native(amount) | Event::Mock(amount) => {
                if amount > budget - progress {
                    end = End::InvalidNative;
                    break;
                }
                if matches!(event, Event::Native(_)) {
                    counts = counts.add_native(amount).unwrap();
                    native = native.checked_add(amount).unwrap();
                } else {
                    counts = counts.add_pair(amount).unwrap();
                }
            }
            Event::Hook => {
                end = End::Hook;
                break;
            }
            Event::Atomic | Event::AtomicError | Event::AtomicPanic => {
                counts = counts.add_dispatch().unwrap();
                end = match event {
                    Event::Atomic => End::Atomic,
                    Event::AtomicError => End::Error,
                    Event::AtomicPanic => End::Panic,
                    _ => unreachable!(),
                };
                break;
            }
            _ => {
                counts = counts.add_pair(1).unwrap();
                match event {
                    Event::Complete => {}
                    Event::Terminal => end = End::Terminal,
                    Event::Error => end = End::Error,
                    Event::Panic => end = End::Panic,
                    Event::Handoff => {
                        counts = counts.rollback_dispatch().unwrap();
                        end = End::Handoff;
                    }
                    _ => unreachable!(),
                }
                if !matches!(event, Event::Complete) {
                    exclude_terminal = true;
                    break;
                }
            }
        }
        if counts.parts().1 >= budget {
            end = End::Budget;
            break;
        }
    }
    let (dispatches, tentative) = counts.parts();
    let progress = tentative.checked_sub(u32::from(exclude_terminal)).unwrap();
    observation(dispatches, progress, native, end)
}

#[test]
fn full_width_parts_and_checked_updates_match_wide_independent_arithmetic() {
    let values = [0, 1, 2, 63, 64, 65, 1 << 31, u32::MAX - 1, u32::MAX];
    for dispatches in values {
        for progress in values {
            let counts = Counts::new(dispatches, progress);
            assert_eq!(counts.parts(), (dispatches, progress));
            for amount in values {
                let d = u128::from(dispatches) + u128::from(amount);
                let p = u128::from(progress) + u128::from(amount);
                let fits = |n| u32::try_from(n).ok();
                assert_eq!(
                    counts.add_pair(amount).map(Counts::parts),
                    fits(d).zip(fits(p))
                );
                assert_eq!(
                    counts.add_native(amount).map(Counts::parts),
                    fits(p).map(|p| (dispatches, p))
                );
                assert_eq!(
                    counts.add_native_dispatch(amount).map(Counts::parts),
                    fits(u128::from(dispatches) + 1).zip(fits(p))
                );
                assert_eq!(
                    counts.add_native_dispatch(amount),
                    counts.add_native(amount).and_then(Counts::add_dispatch)
                );
            }
            assert_eq!(
                counts.add_dispatch().map(Counts::parts),
                dispatches.checked_add(1).map(|d| (d, progress))
            );
            assert_eq!(
                counts.rollback_dispatch().map(Counts::parts),
                dispatches.checked_sub(1).map(|d| (d, progress))
            );
        }
    }
    assert_eq!(Counts::new(u32::MAX, u32::MAX).0, u64::MAX);
}

#[test]
fn terminal_errors_hooks_native_prefixes_and_rollbacks_match_separate_counters() {
    let choices = [
        Event::Native(0),
        Event::Native(1),
        Event::Native(u32::MAX),
        Event::Mock(1),
        Event::Mock(u32::MAX),
        Event::Complete,
        Event::Terminal,
        Event::Error,
        Event::Panic,
        Event::Handoff,
        Event::Atomic,
        Event::AtomicError,
        Event::AtomicPanic,
        Event::Hook,
    ];
    let mut examined = 0;
    let depth = if cfg!(miri) { 2 } else { 3 };
    for budget in [1, 2, 7, 64, u32::MAX - 1, u32::MAX] {
        for prefix in [0, budget - 1] {
            for index in 0..choices.len().pow(depth) {
                let mut index = index;
                let events: Vec<_> = (0..depth)
                    .map(|_| {
                        let event = choices[index % choices.len()];
                        index /= choices.len();
                        event
                    })
                    .collect();
                assert_eq!(
                    paired(prefix, budget, &events),
                    separate(prefix, budget, &events),
                    "{prefix} {budget} {events:?}"
                );
                examined += 1;
            }
        }
    }
    assert_eq!(
        paired(0, 0, &[Event::Complete]),
        separate(0, 0, &[Event::Complete])
    );
    eprintln!("paired_progress_traces={examined}");
}

#[test]
fn explicit_exit_observations_preserve_pre_effect_and_no_guard_boundaries() {
    for run in [separate, paired] {
        assert_eq!(
            run(0, 0, &[Event::Error]),
            Observation {
                dispatches: 0,
                reported: None,
                returned: Some(0),
                end: End::Budget
            }
        );
        for event in [
            Event::Error,
            Event::Panic,
            Event::AtomicError,
            Event::AtomicPanic,
        ] {
            let result = run(0, 64, &[event]);
            assert_eq!(
                (result.dispatches, result.reported, result.returned),
                (1, None, None)
            );
        }
        assert_eq!(
            run(0, 64, &[Event::Terminal]),
            Observation {
                dispatches: 1,
                reported: Some(0),
                returned: Some(0),
                end: End::Terminal
            }
        );
        assert_eq!(
            run(0, 64, &[Event::Handoff]),
            Observation {
                dispatches: 0,
                reported: Some(0),
                returned: Some(0),
                end: End::Handoff
            }
        );
        assert_eq!(
            run(0, 1, &[Event::Complete]),
            Observation {
                dispatches: 1,
                reported: Some(1),
                returned: Some(1),
                end: End::Budget
            }
        );
        assert_eq!(
            run(0, u32::MAX, &[Event::Mock(u32::MAX)]),
            Observation {
                dispatches: u32::MAX,
                reported: Some(u32::MAX),
                returned: Some(u32::MAX),
                end: End::Budget
            }
        );
        assert_eq!(
            run(0, u32::MAX, &[Event::Native(u32::MAX)]),
            Observation {
                dispatches: 0,
                reported: Some(0),
                returned: Some(u32::MAX),
                end: End::Budget
            }
        );
    }
}

#[test]
fn pair_storage_preserves_both_complete_u32_domains() {
    assert_eq!(
        std::mem::size_of::<Counts>(),
        std::mem::size_of::<(u32, u32)>()
    );
    assert_eq!(Counts::new(u32::MAX, 0).parts(), (u32::MAX, 0));
    assert_eq!(Counts::new(0, u32::MAX).parts(), (0, u32::MAX));
}
