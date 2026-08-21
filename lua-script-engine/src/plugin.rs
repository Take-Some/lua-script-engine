use abi_stable::sabi_trait::TD_Opaque;
use abi_stable::std_types::{RResult, RString, RVec};
use newengine_plugin_api::prelude::*;
use newengine_scripting_api::{SCRIPTING_BACKEND_CAPABILITY_ID, SCRIPTING_BACKEND_SERVICE_SPEC};

use crate::runtime::{reset_runtime, shutdown_runtime};
use crate::service::LuaScriptingService;

pub const LUA_PLUGIN_ID: &str = "engine.scripting.lua";
pub const LUA_PLUGIN_NAME: &str = "Lua Script Engine";
pub const LUA_PROVIDER_SERVICE_ID: &str = "scripting.lua.api";
const LUA_PROVIDER_ROUTE: &str = "engine.scripting.lua";

const CONFIG_CONTENT_TYPE: &str = "application/json";
const CONFIG_FORMAT_VERSION: u32 = 1;
const DEFAULT_CONFIG_JSON: &str = r#"{"enabled":true}"#;

fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                if value.is_null() {
                    base.remove(key);
                } else {
                    merge_json(
                        base.entry(key.clone()).or_insert(serde_json::Value::Null),
                        value,
                    );
                }
            }
        }
        (base, patch) => *base = patch.clone(),
    }
}

#[derive(Default)]
pub struct LuaScriptingPlugin;

impl PluginModule for LuaScriptingPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::builder(
            LUA_PLUGIN_ID,
            LUA_PLUGIN_NAME,
            env!("CARGO_PKG_VERSION"),
            PluginKind::Runtime,
        )
        .provides_service(
            LUA_PROVIDER_SERVICE_ID,
            1,
            r#"{"role":"scripting-backend","language":"lua","runtime":"5.4"}"#,
        )
        .push(CapabilityDesc::backend_route(
            SCRIPTING_BACKEND_CAPABILITY_ID,
            BackendRouteDescriptor::new(SCRIPTING_BACKEND_SERVICE_SPEC)
                .contract(LUA_PROVIDER_SERVICE_ID)
                .provider_route(LUA_PROVIDER_ROUTE)
                .backend("lua54")
                .mode("binary-opaque")
                .priority(200)
                .features([
                    "ysc-module-bytes",
                    "lua54-vendored",
                    "compiled-module-cache",
                    "module-export-table",
                    "no-direct-world-access",
                ]),
        ))
        .build()
    }

    fn config_defaults(&self) -> RResult<ConfigBlobV1, RString> {
        RResult::ROk(ConfigBlobV1 {
            content_type: RString::from(CONFIG_CONTENT_TYPE),
            bytes: RVec::from(DEFAULT_CONFIG_JSON.as_bytes().to_vec()),
            format_version: CONFIG_FORMAT_VERSION,
        })
    }

    fn config_apply_patches(
        &self,
        base: &ConfigBlobV1,
        patches: RVec<ConfigPatchV1>,
    ) -> RResult<ConfigApplyResultV1, RString> {
        if base.content_type.as_str() != CONFIG_CONTENT_TYPE {
            return RResult::RErr(RString::from(
                "lua scripting: unsupported config content type",
            ));
        }
        let mut value = match serde_json::from_slice::<serde_json::Value>(base.bytes.as_slice()) {
            Ok(value) => value,
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        let mut changed = false;
        for patch in patches.iter() {
            let patch_value =
                match serde_json::from_slice::<serde_json::Value>(patch.bytes.as_slice()) {
                    Ok(value) => value,
                    Err(error) => return RResult::RErr(RString::from(error.to_string())),
                };
            merge_json(&mut value, &patch_value);
            changed = true;
        }
        let bytes = match serde_json::to_vec(&value) {
            Ok(bytes) => RVec::from(bytes),
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        RResult::ROk(ConfigApplyResultV1 {
            effective: ConfigBlobV1 {
                content_type: RString::from(CONFIG_CONTENT_TYPE),
                bytes,
                format_version: CONFIG_FORMAT_VERSION,
            },
            diags: RVec::new(),
            changed,
        })
    }

    fn config_supports_live_update(&self) -> bool {
        false
    }

    fn config_update_live(
        &mut self,
        _effective: &ConfigBlobV1,
    ) -> RResult<RVec<ConfigDiagV1>, RString> {
        RResult::RErr(RString::from(
            "lua scripting: live config update is not supported in v0.1",
        ))
    }

    fn init(&mut self, host: HostApiV1, effective: ConfigBlobV1) -> RResult<(), RString> {
        if effective.content_type.as_str() != CONFIG_CONTENT_TYPE {
            return RResult::RErr(RString::from(
                "lua scripting: unsupported config content type",
            ));
        }
        let enabled = serde_json::from_slice::<serde_json::Value>(effective.bytes.as_slice())
            .ok()
            .and_then(|value| value.get("enabled").and_then(|v| v.as_bool()))
            .unwrap_or(true);
        if !enabled {
            (host.log_info)(RString::from("lua scripting: provider disabled by config"));
            return RResult::ROk(());
        }
        if let Err(error) = reset_runtime() {
            return RResult::RErr(RString::from(format!(
                "lua scripting: VM initialization failed: {error}"
            )));
        }
        let service: ServiceV1Dyn<'static> =
            ServiceV1_TO::from_value(LuaScriptingService, TD_Opaque);
        if let Err(error) = (host.register_service_v1)(service).into_result() {
            shutdown_runtime();
            return RResult::RErr(RString::from(format!(
                "lua scripting: register_service_v1 failed: {error}"
            )));
        }
        (host.log_info)(RString::from(
            "lua scripting: Lua 5.4 provider initialized route='engine.scripting.lua' module_contract='one .ysc = one module'",
        ));
        RResult::ROk(())
    }

    fn start(&mut self) -> RResult<(), RString> {
        RResult::ROk(())
    }

    fn fixed_update(&mut self, _dt: f32) -> RResult<(), RString> {
        RResult::ROk(())
    }

    fn update(&mut self, _dt: f32) -> RResult<(), RString> {
        RResult::ROk(())
    }

    fn render(&mut self, _dt: f32) -> RResult<(), RString> {
        RResult::ROk(())
    }

    fn shutdown(&mut self) {
        shutdown_runtime();
    }
}
