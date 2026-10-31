//! Provider registry and discovery.
//!
//! Registration is static: adapters compiled into the binary are
//! registered at startup. Discovery asks every registered adapter to detect
//! its provider through the read-only [`HostProbe`]. Nothing provider
//! specific lives here.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use crate::adapter::{HostProbe, Installation, ProviderAdapter};
use crate::capability::CapabilitySet;
use crate::error::AdapterError;
use crate::ids::ProviderId;
use crate::version::{CONTRACT_VERSION, ContractVersion};

/// Registered provider adapters, keyed by provider id.
pub struct ProviderRegistry {
    host_contract: ContractVersion,
    adapters: BTreeMap<ProviderId, Arc<dyn ProviderAdapter>>,
}

impl ProviderRegistry {
    /// Creates an empty registry for the contract version of this build.
    pub fn new() -> Self {
        Self::with_host_contract(CONTRACT_VERSION)
    }

    /// Creates an empty registry that accepts adapters compatible with
    /// `host_contract`. Mainly useful for compatibility tests.
    pub fn with_host_contract(host_contract: ContractVersion) -> Self {
        Self {
            host_contract,
            adapters: BTreeMap::new(),
        }
    }

    /// Registers an adapter after checking its descriptor.
    pub fn register(&mut self, adapter: Arc<dyn ProviderAdapter>) -> Result<(), RegistryError> {
        let descriptor = adapter.descriptor();
        let id = descriptor.id.clone();
        if !descriptor.contract.is_compatible_with(self.host_contract) {
            return Err(RegistryError::IncompatibleContract {
                provider: id,
                adapter: descriptor.contract,
                host: self.host_contract,
            });
        }
        if descriptor.transports.is_empty() {
            return Err(RegistryError::NoTransports(id));
        }
        if self.adapters.contains_key(&id) {
            return Err(RegistryError::Duplicate(id));
        }
        self.adapters.insert(id, adapter);
        Ok(())
    }

    /// Looks up an adapter.
    pub fn get(&self, id: &ProviderId) -> Option<&Arc<dyn ProviderAdapter>> {
        self.adapters.get(id)
    }

    /// Registered provider ids in stable order.
    pub fn ids(&self) -> impl Iterator<Item = &ProviderId> {
        self.adapters.keys()
    }

    /// Number of registered adapters.
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    /// Whether no adapter is registered.
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Runs detection for every registered adapter.
    pub fn discover(&self, probe: &dyn HostProbe) -> Vec<DiscoveryReport> {
        self.adapters
            .iter()
            .map(|(id, adapter)| {
                let outcome = adapter.detect(probe).map(|installation| {
                    let capabilities = adapter.capabilities(&installation);
                    Discovered {
                        installation,
                        capabilities,
                    }
                });
                DiscoveryReport {
                    provider: id.clone(),
                    display_name: adapter.descriptor().display_name.clone(),
                    outcome,
                }
            })
            .collect()
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("host_contract", &self.host_contract)
            .field("providers", &self.adapters.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// A provider found on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Detected installation.
    pub installation: Installation,
    /// Capabilities stated by the adapter for that installation.
    pub capabilities: CapabilitySet,
}

/// Result of detecting one provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryReport {
    /// Provider id.
    pub provider: ProviderId,
    /// Display name.
    pub display_name: String,
    /// Detection outcome.
    pub outcome: Result<Discovered, AdapterError>,
}

/// Registration failures.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// An adapter with this id is already registered.
    Duplicate(ProviderId),
    /// The adapter was built against an incompatible contract version.
    IncompatibleContract {
        /// Provider id.
        provider: ProviderId,
        /// Adapter contract version.
        adapter: ContractVersion,
        /// Host contract version.
        host: ContractVersion,
    },
    /// The adapter declares no transport.
    NoTransports(ProviderId),
}

impl fmt::Display for RegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "provider {id} is already registered"),
            Self::IncompatibleContract {
                provider,
                adapter,
                host,
            } => write!(
                f,
                "provider {provider} targets contract {adapter}, host provides {host}"
            ),
            Self::NoTransports(id) => write!(f, "provider {id} declares no transport"),
        }
    }
}

impl std::error::Error for RegistryError {}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::adapter::{
        EventDecoder, LaunchPlan, LaunchRequest, Platform, ProbeError, ProbeOutput,
        ProviderDescriptor,
    };
    use crate::capability::{Capability, Evidence, Support};
    use crate::error::ErrorClass;
    use crate::permission::{PermissionMapping, PermissionPolicy};
    use crate::transport::{TransportKind, TransportSpec};

    struct FakeProbe {
        installed: bool,
    }

    impl HostProbe for FakeProbe {
        fn find_executable(&self, name: &str) -> Option<PathBuf> {
            self.installed
                .then(|| std::env::temp_dir().join("bin").join(name))
        }

        fn run(
            &self,
            _program: &Path,
            _args: &[&str],
            _timeout: Duration,
        ) -> Result<ProbeOutput, ProbeError> {
            Err(ProbeError::NotFound)
        }

        fn env_var(&self, _name: &str) -> Option<OsString> {
            None
        }

        fn platform(&self) -> Platform {
            Platform::current()
        }
    }

    struct StubAdapter {
        descriptor: ProviderDescriptor,
    }

    impl StubAdapter {
        fn new(id: &str) -> Self {
            Self {
                descriptor: ProviderDescriptor::new(
                    ProviderId::new(id).unwrap(),
                    format!("Stub {id}"),
                    vec![TransportKind::NativePty],
                ),
            }
        }
    }

    impl ProviderAdapter for StubAdapter {
        fn descriptor(&self) -> &ProviderDescriptor {
            &self.descriptor
        }

        fn detect(&self, probe: &dyn HostProbe) -> Result<Installation, AdapterError> {
            probe
                .find_executable(self.descriptor.id.as_str())
                .map(Installation::new)
                .ok_or_else(|| AdapterError::new(ErrorClass::NotInstalled, "not on PATH"))
        }

        fn capabilities(&self, _installation: &Installation) -> CapabilitySet {
            CapabilitySet::new().with(
                Capability::NativeTerminalUi,
                Support::Supported,
                Evidence::Declared,
            )
        }

        fn map_permissions(
            &self,
            _installation: &Installation,
            requested: &PermissionPolicy,
        ) -> PermissionMapping {
            PermissionMapping::new(*requested)
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

        fn event_decoder(&self, _transport: TransportKind) -> Option<Box<dyn EventDecoder>> {
            None
        }
    }

    #[test]
    fn registers_and_lists_in_stable_order() {
        let mut registry = ProviderRegistry::new();
        assert!(registry.is_empty());
        registry
            .register(Arc::new(StubAdapter::new("zeta")))
            .unwrap();
        registry
            .register(Arc::new(StubAdapter::new("alpha")))
            .unwrap();
        let ids: Vec<&str> = registry.ids().map(ProviderId::as_str).collect();
        assert_eq!(ids, ["alpha", "zeta"]);
        assert_eq!(registry.len(), 2);
        assert!(registry.get(&ProviderId::new("alpha").unwrap()).is_some());
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(StubAdapter::new("dup")))
            .unwrap();
        assert_eq!(
            registry.register(Arc::new(StubAdapter::new("dup"))),
            Err(RegistryError::Duplicate(ProviderId::new("dup").unwrap()))
        );
    }

    #[test]
    fn incompatible_contract_is_rejected() {
        let mut registry = ProviderRegistry::with_host_contract(ContractVersion::new(9, 0));
        assert!(matches!(
            registry.register(Arc::new(StubAdapter::new("old"))),
            Err(RegistryError::IncompatibleContract { .. })
        ));
    }

    #[test]
    fn adapter_without_transport_is_rejected() {
        let mut adapter = StubAdapter::new("empty");
        adapter.descriptor.transports.clear();
        let mut registry = ProviderRegistry::new();
        assert!(matches!(
            registry.register(Arc::new(adapter)),
            Err(RegistryError::NoTransports(_))
        ));
    }

    #[test]
    fn discovery_reports_installed_and_missing_providers() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(StubAdapter::new("present")))
            .unwrap();

        let found = registry.discover(&FakeProbe { installed: true });
        let discovered = found[0].outcome.as_ref().unwrap();
        assert_eq!(
            discovered
                .capabilities
                .support(&Capability::NativeTerminalUi),
            Support::Supported
        );

        let missing = registry.discover(&FakeProbe { installed: false });
        assert_eq!(
            missing[0].outcome.as_ref().unwrap_err().class(),
            ErrorClass::NotInstalled
        );
    }

    #[test]
    fn debug_lists_provider_ids() {
        let mut registry = ProviderRegistry::new();
        registry
            .register(Arc::new(StubAdapter::new("visible")))
            .unwrap();
        assert!(format!("{registry:?}").contains("visible"));
    }
}
