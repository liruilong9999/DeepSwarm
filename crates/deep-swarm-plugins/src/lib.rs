use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

wasmtime::component::bindgen!({
    path: "wit/plugin-v1.wit",
    world: "plugin-world",
});

const ABI_VERSION: &str = "deepswarm:plugin@1.0.0";
const LOG_CAPABILITY: &str = "log";
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const MAX_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const MAX_FUEL: u64 = 10_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub abi: String,
    pub wit_sha256: String,
    pub capabilities: Vec<String>,
}

impl PluginManifest {
    pub fn v1() -> Self {
        Self {
            abi: ABI_VERSION.to_owned(),
            wit_sha256: wit_sha256(),
            capabilities: vec![LOG_CAPABILITY.to_owned()],
        }
    }

    pub fn validate(&self) -> Result<(), PluginError> {
        if self.abi != ABI_VERSION || self.wit_sha256 != wit_sha256() {
            return Err(PluginError::IncompatibleAbi);
        }
        if self.capabilities != [LOG_CAPABILITY] {
            return Err(PluginError::UnknownCapability);
        }
        Ok(())
    }
}

pub fn wit_sha256() -> String {
    format!(
        "{:x}",
        Sha256::digest(include_bytes!("../wit/plugin-v1.wit"))
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PluginEventKind {
    SessionStart,
    ToolCall,
    AssertionFail,
    SessionEnd,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginEvent {
    pub kind: PluginEventKind,
    pub payload_json: String,
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("plugin ABI or WIT hash is incompatible")]
    IncompatibleAbi,
    #[error("plugin declares an unsupported capability")]
    UnknownCapability,
    #[error("plugin payload exceeds 64 KiB")]
    PayloadTooLarge,
    #[error("plugin execution timed out")]
    Timeout,
    #[error("plugin returned an oversized error")]
    ErrorTooLarge,
    #[error("plugin rejected the call: {0}")]
    Rejected(String),
    #[error("plugin runtime failed: {0}")]
    Runtime(String),
}

struct HostState {
    limits: StoreLimits,
    logs: Vec<(String, String)>,
}

impl deepswarm::plugin::host::Host for HostState {
    fn log(&mut self, level: String, message: String) {
        let level = match level.as_str() {
            "trace" | "debug" | "info" | "warn" | "error" => level,
            _ => "info".to_owned(),
        };
        let message = redact_and_truncate(&message, MAX_ERROR_BYTES);
        self.logs.push((level, message));
    }
}

pub struct PluginHost {
    engine: Engine,
}

impl PluginHost {
    pub fn new() -> Result<Self, PluginError> {
        let mut config = Config::new();
        config
            .wasm_component_model(true)
            .consume_fuel(true)
            .epoch_interruption(true);
        Engine::new(&config)
            .map(|engine| Self { engine })
            .map_err(runtime_error)
    }

    pub fn load(
        &self,
        component_bytes: &[u8],
        manifest: &PluginManifest,
    ) -> Result<PluginInstance, PluginError> {
        manifest.validate()?;
        let component =
            Component::from_binary(&self.engine, component_bytes).map_err(runtime_error)?;
        let mut linker = Linker::new(&self.engine);
        PluginWorld::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(runtime_error)?;
        let state = HostState {
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_MEMORY_BYTES)
                .build(),
            logs: Vec::new(),
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store.set_fuel(MAX_FUEL).map_err(runtime_error)?;
        store.set_epoch_deadline(1);
        let bindings =
            PluginWorld::instantiate(&mut store, &component, &linker).map_err(runtime_error)?;
        Ok(PluginInstance {
            engine: self.engine.clone(),
            store,
            bindings,
        })
    }
}

pub struct PluginInstance {
    engine: Engine,
    store: Store<HostState>,
    bindings: PluginWorld,
}

impl PluginInstance {
    pub fn init(&mut self, config_json: &str) -> Result<String, PluginError> {
        validate_payload(config_json)?;
        self.prepare_call();
        let done = self.interrupt_after(Duration::from_secs(1));
        let result = self
            .bindings
            .deepswarm_plugin_plugin()
            .call_init(&mut self.store, config_json);
        done.store(true, Ordering::Release);
        classify_result(result)
    }

    pub fn on_event(&mut self, event: &PluginEvent) -> Result<String, PluginError> {
        validate_payload(&event.payload_json)?;
        let event = exports::deepswarm::plugin::plugin::Event {
            kind: match event.kind {
                PluginEventKind::SessionStart => {
                    exports::deepswarm::plugin::plugin::EventKind::SessionStart
                }
                PluginEventKind::ToolCall => {
                    exports::deepswarm::plugin::plugin::EventKind::ToolCall
                }
                PluginEventKind::AssertionFail => {
                    exports::deepswarm::plugin::plugin::EventKind::AssertionFail
                }
                PluginEventKind::SessionEnd => {
                    exports::deepswarm::plugin::plugin::EventKind::SessionEnd
                }
            },
            payload_json: event.payload_json.clone(),
        };
        self.prepare_call();
        let done = self.interrupt_after(Duration::from_millis(100));
        let result = self
            .bindings
            .deepswarm_plugin_plugin()
            .call_on_event(&mut self.store, &event);
        done.store(true, Ordering::Release);
        classify_result(result)
    }

    pub fn shutdown(&mut self) -> Result<String, PluginError> {
        self.prepare_call();
        let done = self.interrupt_after(Duration::from_secs(1));
        let result = self
            .bindings
            .deepswarm_plugin_plugin()
            .call_shutdown(&mut self.store);
        done.store(true, Ordering::Release);
        classify_result(result)
    }

    pub fn logs(&self) -> &[(String, String)] {
        &self.store.data().logs
    }

    fn prepare_call(&mut self) {
        self.store.set_epoch_deadline(1);
        let _ = self.store.set_fuel(MAX_FUEL);
    }

    fn interrupt_after(&self, timeout: Duration) -> Arc<AtomicBool> {
        let engine = self.engine.clone();
        let done = Arc::new(AtomicBool::new(false));
        let timer_done = done.clone();
        std::thread::spawn(move || {
            std::thread::sleep(timeout);
            if !timer_done.load(Ordering::Acquire) {
                engine.increment_epoch();
            }
        });
        done
    }
}

fn classify_result(
    result: wasmtime::Result<Result<String, String>>,
) -> Result<String, PluginError> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(message)) if message.len() > MAX_ERROR_BYTES => Err(PluginError::ErrorTooLarge),
        Ok(Err(message)) => Err(PluginError::Rejected(redact_and_truncate(
            &message,
            MAX_ERROR_BYTES,
        ))),
        Err(error)
            if error.to_string().contains("epoch deadline")
                || error.to_string().contains("interrupt") =>
        {
            Err(PluginError::Timeout)
        }
        Err(error) => Err(runtime_error(error)),
    }
}

fn validate_payload(value: &str) -> Result<(), PluginError> {
    (value.len() <= MAX_PAYLOAD_BYTES)
        .then_some(())
        .ok_or(PluginError::PayloadTooLarge)
}

fn redact_and_truncate(value: &str, max_bytes: usize) -> String {
    let redacted = value
        .split_whitespace()
        .map(|part| {
            if part.starts_with("sk-") {
                "[REDACTED]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if redacted.len() <= max_bytes {
        return redacted;
    }
    let mut end = max_bytes;
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    redacted[..end].to_owned()
}

fn runtime_error(error: impl std::fmt::Display) -> PluginError {
    PluginError::Runtime(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Command};

    use super::*;

    #[test]
    fn manifest_is_tied_to_the_checked_in_wit() {
        PluginManifest::v1().validate().unwrap();
        let mut manifest = PluginManifest::v1();
        manifest.wit_sha256.push('0');
        assert!(matches!(
            manifest.validate(),
            Err(PluginError::IncompatibleAbi)
        ));
    }

    #[test]
    fn unknown_capabilities_are_rejected() {
        let mut manifest = PluginManifest::v1();
        manifest.capabilities.push("network".into());
        assert!(matches!(
            manifest.validate(),
            Err(PluginError::UnknownCapability)
        ));
    }

    #[test]
    fn payload_limit_and_secret_redaction_are_enforced() {
        assert!(matches!(
            validate_payload(&"x".repeat(MAX_PAYLOAD_BYTES + 1)),
            Err(PluginError::PayloadTooLarge)
        ));
        assert_eq!(redact_and_truncate("key sk-secret", 100), "key [REDACTED]");
    }

    #[test]
    fn compatible_component_runs_all_lifecycle_hooks() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let guest_dir = manifest_dir.join("test-guest");
        let status = Command::new("cargo")
            .env("RUSTUP_TOOLCHAIN", "stable")
            .args(["build", "--release", "--target", "wasm32-unknown-unknown"])
            .current_dir(&guest_dir)
            .status()
            .expect("cargo must be available");
        assert!(status.success(), "test guest must compile");

        let core_module = std::fs::read(
            guest_dir
                .join("target/wasm32-unknown-unknown/release/deep_swarm_plugin_test_guest.wasm"),
        )
        .unwrap();
        let bytes = wit_component::ComponentEncoder::default()
            .module(&core_module)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap();
        let host = PluginHost::new().unwrap();
        let mut plugin = host.load(&bytes, &PluginManifest::v1()).unwrap();
        assert_eq!(
            plugin.init(r#"{"mode":"test"}"#).unwrap(),
            r#"{"mode":"test"}"#
        );
        assert_eq!(
            plugin
                .on_event(&PluginEvent {
                    kind: PluginEventKind::ToolCall,
                    payload_json: r#"{"tool":"diagnostics"}"#.into(),
                })
                .unwrap(),
            r#"{"tool":"diagnostics"}"#
        );
        assert_eq!(plugin.shutdown().unwrap(), "shutdown");
        assert_eq!(
            plugin.logs(),
            &[("info".into(), "test plugin initialized".into())]
        );
    }
}
