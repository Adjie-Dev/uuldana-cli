//! A complete provider adapter, registered and discovered the way Uuldana's own
//! adapters are. It describes a program called `echo-agent`; it does not need to
//! exist, so discovery reports it as not installed.
//!
//! ```text
//! cargo run -p uuldana-provider-sdk --example minimal_adapter
//! ```

use std::{path::PathBuf, sync::Arc, time::Duration};

use uuldana_core::ProviderRegistry;
use uuldana_provider_sdk::prelude::*;
use uuldana_provider_sdk::{ApprovalMode, FilesystemAccess, NetworkAccess};

struct EchoAgent {
    descriptor: ProviderDescriptor,
}

impl EchoAgent {
    fn new() -> Self {
        Self {
            descriptor: ProviderDescriptor::new(
                ProviderId::new("echo-agent").expect("valid id"),
                "Echo Agent",
                vec![TransportKind::NativePty],
            ),
        }
    }
}

impl ProviderAdapter for EchoAgent {
    fn descriptor(&self) -> &ProviderDescriptor {
        &self.descriptor
    }

    // Find the program and read its version. Nothing else.
    fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
        let exe = probe.find_executable("echo-agent").ok_or_else(|| {
            AdapterError::new(ErrorClass::NotInstalled, "echo-agent not found on PATH")
        })?;
        let out = probe.run(&exe, &["--version"], Duration::from_secs(5))?;
        let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        Ok(Installation::new(exe).with_version(version))
    }

    // Only claim what you can show. Everything else stays unknown.
    fn capabilities(&self, _: &Installation) -> CapabilitySet {
        CapabilitySet::new()
            .with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
            .with(
                Capability::NetworkSandboxControl,
                Support::Unsupported,
                Evidence::Declared,
            )
    }

    // Be honest: this agent cannot block the network, so it reports the
    // permissive value and the core refuses a policy that asked for more.
    fn map_permissions(&self, _: &Installation, requested: &PermissionPolicy) -> PermissionMapping {
        let mut effective = *requested;
        effective.sandbox.network = NetworkAccess::Allowed;
        PermissionMapping::new(effective).with_note("no network sandbox")
    }

    fn launch_plan(
        &self,
        installation: &Installation,
        request: &LaunchRequest,
    ) -> Result<LaunchPlan, AdapterError> {
        Ok(LaunchPlan::new(
            installation.executable.clone(),
            request.workspace.clone(),
            TransportSpec::NativePty {
                initial_size: request.terminal_size.unwrap_or_default(),
            },
        ))
    }

    fn event_decoder(&self, _: TransportKind) -> Option<Box<dyn EventDecoder>> {
        None
    }
}

fn main() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(EchoAgent::new()))
        .expect("registers");

    struct NoHost;
    impl HostProbe for NoHost {
        fn find_executable(&self, _: &str) -> Option<PathBuf> {
            None
        }
        fn run(
            &self,
            _: &std::path::Path,
            _: &[&str],
            _: Duration,
        ) -> Result<uuldana_provider_sdk::ProbeOutput, uuldana_provider_sdk::ProbeError> {
            Err(uuldana_provider_sdk::ProbeError::NotFound)
        }
        fn env_var(&self, _: &str) -> Option<std::ffi::OsString> {
            None
        }
        fn platform(&self) -> uuldana_provider_sdk::Platform {
            uuldana_provider_sdk::Platform::current()
        }
    }

    for report in registry.discover(&NoHost) {
        match report.outcome {
            Ok(found) => println!(
                "{}: installed at {}",
                report.provider,
                found.installation.executable.display()
            ),
            Err(error) => println!("{}: {error}", report.provider),
        }
    }

    // The permission mapping, as the supervisor sees it.
    let adapter = EchoAgent::new();
    let installation = Installation::new(PathBuf::from("echo-agent"));
    let requested = PermissionPolicy {
        approval: ApprovalMode::AskForRisky,
        sandbox: uuldana_provider_sdk::SandboxPolicy {
            filesystem: FilesystemAccess::WorkspaceWrite,
            network: NetworkAccess::Denied,
        },
    };
    let mapped = adapter.map_permissions(&installation, &requested);
    match uuldana_provider_sdk::validate_mapping(&requested, &mapped) {
        Ok(_) => println!("policy accepted"),
        Err(error) => println!("policy refused: {error}"),
    }
}
