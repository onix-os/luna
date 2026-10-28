use super::resources::{BudgetAllocator, Ledger};
use super::{compiler_request as request, compiler_service as service, compiler_stream as stream};

fn allocator() -> Result<BudgetAllocator, u8> {
    Ledger::try_host(32 << 20)
        .map(BudgetAllocator)
        .map_err(|_| 65)
}

fn configuration() -> Result<stream::Service, u8> {
    for (resource, maximum) in [
        (libc::RLIMIT_AS, 512 << 20),
        (libc::RLIMIT_CPU, 3),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(resource, &mut limit) } != 0
            || limit.rlim_cur > maximum
            || limit.rlim_max > maximum
            || (resource != libc::RLIMIT_CORE && limit.rlim_cur == 0)
        {
            return Err(64);
        }
    }
    let worker = request::worker_identity().map_err(|_| 64)?;
    Ok(stream::Service {
        worker,
        abi: request::abi_identity(),
        request_limit: request::MAX_BYTES,
        input: allocator()?,
        limits: service::Limits {
            source: super::compiler_wire::Limits {
                bytes: 1 << 20,
                operations: 65536,
                constants: 65536,
            },
            artifact: super::backend::artifact::Limits {
                bytes: 2 << 20,
                modules: 16,
                declarations: 256,
                functions: 128,
                relocations: 8192,
            },
        },
        allocators: service::Allocators {
            source: allocator()?,
            metadata: allocator()?,
            objects: allocator()?,
            output: allocator()?,
        },
    })
}

pub(super) fn run() -> u8 {
    let config = match configuration() {
        Ok(config) => config,
        Err(code) => return code,
    };
    let result = stream::serve(
        &mut std::io::stdin().lock(),
        &mut std::io::stderr().lock(),
        config,
    );
    match result {
        Ok(()) => 0,
        Err(stream::Error::Configuration) => 64,
        Err(stream::Error::Limit | stream::Error::Allocation) => 65,
        Err(stream::Error::Read(_) | stream::Error::Write(_)) => 66,
        Err(stream::Error::Service(_)) => 67,
    }
}
