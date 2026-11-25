use super::*;
use crate::jit::resources::Ledger;

fn allocator() -> BudgetAllocator {
    BudgetAllocator(Ledger::new(8 << 20))
}

struct Input<'a> {
    bytes: &'a [u8],
    step: usize,
    calls: usize,
    interrupted: bool,
}
impl Read for Input<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        self.calls += 1;
        if std::mem::take(&mut self.interrupted) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let count = out.len().min(self.step).min(self.bytes.len());
        out[..count].copy_from_slice(&self.bytes[..count]);
        self.bytes = &self.bytes[count..];
        Ok(count)
    }
}
fn input(bytes: &[u8], step: usize) -> Input<'_> {
    Input {
        bytes,
        step,
        calls: 0,
        interrupted: true,
    }
}

#[derive(Default)]
struct Output {
    bytes: std::vec::Vec<u8>,
    step: usize,
    calls: usize,
    flushes: usize,
    interrupted: bool,
    flush_interrupted: bool,
    stop: Option<usize>,
    zero: bool,
    flush_error: bool,
}
impl Write for Output {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.calls += 1;
        if std::mem::take(&mut self.interrupted) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        let remaining = self
            .stop
            .unwrap_or(usize::MAX)
            .saturating_sub(self.bytes.len());
        if remaining == 0 {
            return if self.zero {
                Ok(0)
            } else {
                Err(io::Error::from_raw_os_error(libc::EPIPE))
            };
        }
        let count = bytes.len().min(self.step).min(remaining);
        self.bytes.extend_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        if std::mem::take(&mut self.flush_interrupted) {
            return Err(io::ErrorKind::Interrupted.into());
        }
        if self.flush_error {
            Err(io::ErrorKind::BrokenPipe.into())
        } else {
            Ok(())
        }
    }
}
fn output() -> Output {
    Output {
        step: 7,
        interrupted: true,
        flush_interrupted: true,
        ..Default::default()
    }
}

#[test]
fn fragmented_interrupted_and_empty_reads_preserve_exact_bytes() {
    for source in [&b""[..], &b"packet-bytes"[..]] {
        for step in [1, 3, 4096] {
            let output = allocator();
            let mut reader = input(source, step);
            let bytes = read(&mut reader, source.len(), output.clone()).unwrap();
            assert_eq!(&*bytes, source);
            assert!(reader.bytes.is_empty());
            assert_eq!(output.0.current(), bytes.capacity());
            drop(bytes);
            assert_eq!(output.0.current(), 0);
        }
    }
}

#[test]
fn exact_read_caps_consume_only_one_extra_byte_and_reclaim_on_overflow() {
    for limit in [0, 1, 7, 16] {
        let source = [1; 20];
        let mut reader = input(&source, 4096);
        let output = allocator();
        assert_eq!(read(&mut reader, limit, output.clone()), Err(Error::Limit));
        assert_eq!(reader.bytes.len(), source.len() - limit - 1);
        assert_eq!(output.0.current(), 0);
    }
    let mut reader = input(b"x", 1);
    assert_eq!(
        read(&mut reader, request::MAX_BYTES + 1, allocator()),
        Err(Error::Configuration)
    );
    assert_eq!(reader.calls, 0);
}

#[test]
fn every_read_growth_refusal_reclaims_quota_and_preserves_denial() {
    let mut refused = 0;
    let mut completed = 0;
    for allowance in 0..=8 {
        let source = [3; 130];
        let mut reader = input(&source, 7);
        let output = allocator();
        output.0.fail_after(allowance);
        match read(&mut reader, source.len(), output.clone()) {
            Ok(bytes) => {
                completed += 1;
                assert_eq!(&*bytes, &source);
                drop(bytes);
            }
            Err(error) => {
                refused += 1;
                assert_eq!(error, Error::Allocation);
            }
        }
        assert_eq!(output.0.current(), 0);
        if output.0.refusals() > 0 {
            assert!(Vec::<u8, _>::new_in(output).try_reserve_exact(1).is_err());
        }
    }
    assert!(refused > 0 && completed > 0);
    let output = BudgetAllocator(Ledger::new(1));
    assert_eq!(
        read(&mut &b"ab"[..], 2, output.clone()),
        Err(Error::Allocation)
    );
    assert_eq!(output.0.current(), 0);
}

#[test]
fn read_faults_capture_fixed_os_details_and_do_not_spin_on_would_block() {
    struct Broken(usize);
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            if self.0 == 1 {
                Err(io::ErrorKind::Interrupted.into())
            } else {
                Err(io::Error::from_raw_os_error(libc::EIO))
            }
        }
    }
    let mut reader = Broken(0);
    assert_eq!(
        read(&mut reader, 9, allocator()),
        Err(Error::Read(IoFault {
            kind: io::Error::from_raw_os_error(libc::EIO).kind(),
            os: Some(libc::EIO),
        }))
    );
    assert_eq!(reader.0, 2);
    struct WouldBlock(usize);
    impl Read for WouldBlock {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            Err(io::ErrorKind::WouldBlock.into())
        }
    }
    let mut reader = WouldBlock(0);
    assert_eq!(
        read(&mut reader, 9, allocator()),
        Err(Error::Read(IoFault {
            kind: io::ErrorKind::WouldBlock,
            os: None,
        }))
    );
    assert_eq!(reader.0, 1);
    struct Invalid;
    impl Read for Invalid {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            Ok(bytes.len() + 1)
        }
    }
    assert_eq!(
        read(&mut Invalid, 1, allocator()),
        Err(Error::Read(IoFault {
            kind: io::ErrorKind::InvalidData,
            os: None,
        }))
    );
}

#[test]
fn short_interrupted_writes_and_flushes_complete_both_reply_segments() {
    for outcome in [
        reply::Outcome::Artifact(&b"code"[..]),
        reply::Outcome::Failure(reply::Fault {
            class: reply::Class::Resource,
            stage: reply::Stage::Compiler,
        }),
    ] {
        let header = reply::header(outcome, 13, &[0x12; 32]).unwrap();
        let payload = outcome.into_result().unwrap_or(&[]);
        let mut writer = output();
        send(&mut writer, &header, payload).unwrap();
        assert_eq!(reply::decode(&writer.bytes, 13, &[0x12; 32]), Ok(outcome));
        assert_eq!(writer.flushes, 2);
    }
}

#[test]
fn failures_at_each_header_payload_and_flush_boundary_never_report_success() {
    let header = reply::header(reply::Outcome::Artifact(b"code"), 13, &[0x12; 32]).unwrap();
    for prefix in 0..reply::HEADER + 4 {
        for zero in [false, true] {
            let mut writer = Output {
                stop: Some(prefix),
                zero,
                ..output()
            };
            assert_eq!(
                send(&mut writer, &header, b"code"),
                Err(Error::Write(IoFault {
                    kind: if zero {
                        io::ErrorKind::WriteZero
                    } else {
                        io::ErrorKind::BrokenPipe
                    },
                    os: if zero { None } else { Some(libc::EPIPE) },
                }))
            );
            assert_eq!(writer.bytes.len(), prefix);
            assert_eq!(writer.flushes, 0);
        }
    }
    let mut writer = Output {
        flush_error: true,
        ..output()
    };
    assert_eq!(
        send(&mut writer, &header, b"code"),
        Err(Error::Write(IoFault {
            kind: io::ErrorKind::BrokenPipe,
            os: None,
        }))
    );
    assert_eq!(writer.bytes.len(), reply::HEADER + 4);
    struct Invalid;
    impl Write for Invalid {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len() + 1)
        }
        fn flush(&mut self) -> io::Result<()> {
            panic!("unexpected flush")
        }
    }
    assert_eq!(
        send(&mut Invalid, &header, b"code"),
        Err(Error::Write(IoFault {
            kind: io::ErrorKind::InvalidData,
            os: None,
        }))
    );
}

#[test]
fn invalid_output_configuration_precedes_any_write_or_flush() {
    for header in [
        &[][..],
        &[0; reply::HEADER - 1][..],
        &[0; reply::HEADER + 1][..],
    ] {
        let mut writer = output();
        assert_eq!(send(&mut writer, header, &[]), Err(Error::Configuration));
        assert_eq!(writer.calls, 0);
        assert_eq!(writer.flushes, 0);
    }
}

#[cfg(not(miri))]
mod native {
    use super::*;
    use crate::jit::{backend::artifact, compiler_service as service, compiler_wire as wire, work};

    fn config() -> Service {
        Service {
            worker: [7; 32],
            abi: [8; 32],
            request_limit: request::MAX_BYTES,
            input: allocator(),
            limits: service::Limits {
                source: wire::Limits {
                    bytes: 1 << 20,
                    operations: 65536,
                    constants: 65536,
                },
                artifact: artifact::Limits {
                    bytes: 2 << 20,
                    modules: 16,
                    declarations: 256,
                    functions: 128,
                    relocations: 8192,
                },
            },
            allocators: service::Allocators {
                source: allocator(),
                metadata: allocator(),
                objects: allocator(),
                output: allocator(),
            },
        }
    }
    fn ledgers(config: &Service) -> [BudgetAllocator; 5] {
        [
            config.input.clone(),
            config.allocators.source.clone(),
            config.allocators.metadata.clone(),
            config.allocators.objects.clone(),
            config.allocators.output.clone(),
        ]
    }
    fn packet(work: work::Limits) -> Vec<u8, BudgetAllocator> {
        let config = config();
        let source = crate::Lua::empty().enter(|ctx| {
            let closure = crate::Closure::load(
                ctx,
                None,
                &b"local s=0 for i=1,20 do s=s+i end return s"[..],
            )
            .unwrap();
            let snapshot =
                crate::jit::ir::Snapshot::new(&closure.prototype(), 4096, 2 << 20).unwrap();
            wire::encode(&snapshot, config.limits.source, allocator()).unwrap()
        });
        let features = artifact::target_features().unwrap();
        let environment = artifact::environment_in(allocator()).unwrap();
        request::encode(
            &request::View {
                request: 17,
                worker: &config.worker,
                abi: &config.abi,
                work,
                route: request::Route::Ordinary,
                resume: false,
                features: &features,
                environment: &environment,
                source: &source,
            },
            allocator(),
        )
        .unwrap()
    }

    #[test]
    fn actual_owned_service_handles_fragmented_success_and_resource_replies() {
        for resource in [false, true] {
            let mut work = work::Limits::from(&crate::JitConfig::default());
            if resource {
                work.instructions = 0;
            }
            let packet = packet(work);
            let binding = request::binding(&packet).unwrap();
            let config = config();
            let ledgers = ledgers(&config);
            let mut reader = input(&packet, 3);
            let mut writer = output();
            serve(&mut reader, &mut writer, config).unwrap();
            let result = reply::decode(&writer.bytes, 17, &binding)
                .unwrap()
                .into_result();
            if resource {
                assert_eq!(
                    result,
                    Err(reply::Fault {
                        class: reply::Class::Resource,
                        stage: reply::Stage::Compiler
                    })
                );
            } else {
                assert!(result.unwrap().starts_with(b"LUNACOD\0"));
            }
            for ledger in ledgers {
                assert_eq!(ledger.0.current(), 0);
            }
        }
    }

    #[test]
    fn stream_service_reclaims_ownership_on_input_config_request_and_output_failure() {
        for failure in 0..5 {
            let mut packet = packet(work::Limits::from(&crate::JitConfig::default()));
            let mut config = config();
            let ledgers = ledgers(&config);
            match failure {
                0 => config.input.0.fail_after(0),
                1 => config.limits.artifact.bytes += 1,
                2 => packet[0] ^= 1,
                3 => {}
                _ => config.request_limit += 1,
            }
            let mut reader = input(&packet, 3);
            let mut writer = Output {
                stop: if failure == 3 { Some(1) } else { None },
                ..output()
            };
            let error = serve(&mut reader, &mut writer, config).unwrap_err();
            assert_eq!(
                error,
                match failure {
                    0 => Error::Allocation,
                    1 => Error::Service(service::Error::Configuration),
                    2 => Error::Service(service::Error::Request(request::Error::Header)),
                    3 => Error::Write(IoFault {
                        kind: io::ErrorKind::BrokenPipe,
                        os: Some(libc::EPIPE)
                    }),
                    _ => Error::Configuration,
                }
            );
            if failure == 1 || failure == 4 {
                assert_eq!(reader.calls, 0);
            }
            for ledger in ledgers {
                assert_eq!(ledger.0.current(), 0);
            }
        }
    }

    #[test]
    fn maximum_request_and_reply_caps_are_exact() {
        let bytes = std::vec![0u8;request::MAX_BYTES+1];
        let mut reader = input(&bytes[..request::MAX_BYTES], 4096);
        assert_eq!(
            read(&mut reader, request::MAX_BYTES, allocator())
                .unwrap()
                .len(),
            request::MAX_BYTES
        );
        let mut reader = input(&bytes, 4096);
        assert_eq!(
            read(&mut reader, request::MAX_BYTES, allocator()),
            Err(Error::Limit)
        );
        assert!(reader.bytes.is_empty());
        let payload = std::vec![0;reply::MAX_BYTES-reply::HEADER+1];
        let header = reply::header(
            reply::Outcome::Artifact(&payload[..payload.len() - 1]),
            1,
            &[0; 32],
        )
        .unwrap();
        let mut writer = io::sink();
        send(&mut writer, &header, &payload[..payload.len() - 1]).unwrap();
        assert_eq!(
            send(&mut writer, &header, &payload),
            Err(Error::Configuration)
        );
    }
}
