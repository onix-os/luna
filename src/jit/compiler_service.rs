use allocator_api2::vec::Vec;

use super::{
    backend::artifact, compiler_reply as reply, compiler_request as request, compiler_wire as wire,
    resources::BudgetAllocator,
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub source: wire::Limits,
    pub artifact: artifact::Limits,
}

pub(super) struct Allocators {
    pub source: BudgetAllocator,
    pub metadata: BudgetAllocator,
    pub objects: BudgetAllocator,
    pub output: BudgetAllocator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    Configuration,
    Request(request::Error),
    Reply(reply::Error),
}

impl Limits {
    pub fn validate(self) -> Result<(), Error> {
        if self.artifact.bytes > reply::MAX_BYTES - reply::HEADER {
            Err(Error::Configuration)
        } else {
            Ok(())
        }
    }
}

pub(super) struct Response {
    header: [u8; reply::HEADER],
    payload: Option<Vec<u8, BudgetAllocator>>,
}

impl Response {
    pub fn header(&self) -> &[u8] {
        &self.header
    }

    pub fn payload(&self) -> &[u8] {
        self.payload.as_deref().unwrap_or(&[])
    }
}

fn compile(
    view: &request::View<'_>,
    build: &[u8; 32],
    limits: Limits,
    allocators: Allocators,
) -> Result<Vec<u8, BudgetAllocator>, reply::Fault> {
    let snapshot =
        wire::decode(view.source, limits.source, allocators.source).map_err(|error| {
            reply::Fault {
                class: match error {
                    wire::Error::Allocation | wire::Error::Limit => reply::Class::Resource,
                    _ => reply::Class::Rejected,
                },
                stage: reply::Stage::Snapshot,
            }
        })?;
    let bundle = artifact::with_target(view.features, || {
        artifact::emission::emit(
            &snapshot,
            match view.route {
                request::Route::Ordinary => artifact::Route::Ordinary,
                request::Route::Arrays => artifact::Route::Arrays,
                request::Route::Readonly => artifact::Route::Readonly,
            },
            view.resume,
            view.work,
            allocators.metadata,
            allocators.objects,
        )
    })
    .map_err(|_| reply::Fault {
        class: reply::Class::Rejected,
        stage: reply::Stage::Target,
    })?
    .map_err(|error| reply::Fault::classify(&error, reply::Stage::Compiler))?;
    if bundle.target_environment() != view.environment {
        return Err(reply::Fault {
            class: reply::Class::Rejected,
            stage: reply::Stage::Target,
        });
    }
    artifact::encode(
        &bundle,
        artifact::Binding {
            request: view.request,
            build,
            source: view.source,
            environment: view.environment,
        },
        limits.artifact,
        allocators.output,
    )
    .map_err(|error| reply::Fault {
        class: match error {
            artifact::Error::Allocation | artifact::Error::Limit => reply::Class::Resource,
            _ => reply::Class::Compilation,
        },
        stage: reply::Stage::Artifact,
    })
}

pub(super) fn handle(
    packet: Vec<u8, BudgetAllocator>,
    worker: &[u8; 32],
    abi: &[u8; 32],
    limits: Limits,
    allocators: Allocators,
) -> Result<Response, Error> {
    limits.validate()?;
    let view = request::decode(&packet, worker, abi).map_err(Error::Request)?;
    let build = request::binding(&packet).map_err(Error::Request)?;
    let result = compile(&view, &build, limits, allocators);
    let outcome = match &result {
        Ok(bytes) => reply::Outcome::Artifact(bytes),
        Err(fault) => reply::Outcome::Failure(*fault),
    };
    let header = reply::header(outcome, view.request, &build).map_err(Error::Reply)?;
    Ok(Response {
        header,
        payload: result.ok(),
    })
}
