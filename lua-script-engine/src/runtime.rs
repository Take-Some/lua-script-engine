use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use mlua::{Error as LuaError, Function, HookTriggers, Lua, RegistryKey, Table, Value, VmState};
use newengine_scripting_api::{
    ScriptDiagnostic, ScriptModuleRef, ScriptModuleRefValidationResponse, ScriptModuleState,
    ScriptingModuleLoadBytesRequest, ScriptingModuleLoadBytesResponse, ScriptingModuleRecord,
    ScriptingRequestBytes, ScriptingResponseBytes, ScriptingResponseStatus, ScriptingServiceInfo,
    ScriptingStateDump, ENGINE_SCRIPTING_SERVICE_ID, SCRIPTING_BACKEND_CAPABILITY_ID,
};
use parking_lot::Mutex;

use crate::plugin::LUA_PROVIDER_SERVICE_ID;

const PROVIDER_LABEL: &str = "lua54";
const MAX_MODULE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_INSTRUCTIONS_PER_CALL: u64 = 2_000_000;
const INSTRUCTION_HOOK_INTERVAL: u32 = 10_000;

#[derive(Debug)]
struct LoadedLuaModule {
    record: ScriptingModuleRecord,
    exports: RegistryKey,
}

pub struct LuaRuntimeState {
    lua: Lua,
    modules: BTreeMap<String, LoadedLuaModule>,
    request_count: u64,
    frame_count: u64,
    load_count: u64,
    error_count: u64,
    instruction_budget: Arc<AtomicU64>,
}

impl LuaRuntimeState {
    pub fn new() -> Result<Self, String> {
        let lua = Lua::new();
        let instruction_budget = Arc::new(AtomicU64::new(MAX_INSTRUCTIONS_PER_CALL));
        let hook_budget = Arc::clone(&instruction_budget);
        lua.set_hook(
            HookTriggers::new().every_nth_instruction(INSTRUCTION_HOOK_INTERVAL),
            move |_lua, _debug| {
                let remaining = hook_budget.load(Ordering::Relaxed);
                if remaining <= INSTRUCTION_HOOK_INTERVAL as u64 {
                    hook_budget.store(0, Ordering::Relaxed);
                    return Err(LuaError::RuntimeError(
                        "North Star Lua instruction budget exceeded".to_owned(),
                    ));
                }
                hook_budget.fetch_sub(INSTRUCTION_HOOK_INTERVAL as u64, Ordering::Relaxed);
                Ok(VmState::Continue)
            },
        )
        .map_err(|e| e.to_string())?;
        let globals = lua.globals();
        globals
            .set("_NORTHSTAR_SCRIPT_PROVIDER", PROVIDER_LABEL)
            .map_err(|e| e.to_string())?;
        Ok(Self {
            lua,
            modules: BTreeMap::new(),
            request_count: 0,
            frame_count: 0,
            load_count: 0,
            error_count: 0,
            instruction_budget,
        })
    }

    pub fn service_info(&self) -> ScriptingServiceInfo {
        let mut info = ScriptingServiceInfo {
            provider: LUA_PROVIDER_SERVICE_ID.to_owned(),
            backend: PROVIDER_LABEL.to_owned(),
            ..ScriptingServiceInfo::default()
        };
        info.features = vec![
            "binary-request-response".to_owned(),
            "json-structured-request-response".to_owned(),
            "ysc-module-bytes".to_owned(),
            "lua54-vendored".to_owned(),
            "compiled-module-cache".to_owned(),
            "module-export-table".to_owned(),
            "no-direct-world-access".to_owned(),
            "instruction-budget".to_owned(),
        ];
        info.provider_metadata
            .insert("language".to_owned(), "lua".to_owned());
        info.provider_metadata
            .insert("runtime".to_owned(), "lua-5.4".to_owned());
        info.provider_metadata.insert(
            "max_instructions_per_call".to_owned(),
            MAX_INSTRUCTIONS_PER_CALL.to_string(),
        );
        info.provider_metadata.insert(
            "module_contract".to_owned(),
            "one .ysc asset = one Lua module; module returns export table".to_owned(),
        );
        info
    }

    pub fn validate_module_ref(module_ref: ScriptModuleRef) -> ScriptModuleRefValidationResponse {
        let mut diagnostics = Vec::new();
        let reference = module_ref.reference.trim();
        if reference.is_empty() {
            diagnostics.push(ScriptDiagnostic::error(
                "LUA_SCRIPTING_EMPTY_MODULE_REF",
                "Lua module reference must not be empty.",
            ));
        }
        if reference.contains('@') {
            diagnostics.push(ScriptDiagnostic::error(
                "LUA_SCRIPTING_YSC_SELECTOR_FORBIDDEN",
                ".ysc is one script module, not a dictionary; @entry selectors are forbidden.",
            ));
        }
        if !reference.to_ascii_lowercase().ends_with(".ysc") {
            diagnostics.push(ScriptDiagnostic::error(
                "LUA_SCRIPTING_NOT_YSC",
                "Lua scripting provider accepts canonical .ysc module assets only.",
            ));
        }
        ScriptModuleRefValidationResponse {
            ok: diagnostics.is_empty(),
            module_ref,
            diagnostics,
        }
    }

    pub fn load_module_bytes(
        &mut self,
        request: ScriptingModuleLoadBytesRequest,
    ) -> ScriptingModuleLoadBytesResponse {
        let validation = Self::validate_module_ref(request.module_ref.clone());
        if !validation.ok {
            return failed_load(request.module_ref, validation.diagnostics);
        }
        if request.module_bytes.len() > MAX_MODULE_BYTES {
            return failed_load(
                request.module_ref,
                vec![ScriptDiagnostic::error(
                    "LUA_SCRIPTING_MODULE_TOO_LARGE",
                    format!(
                        "Lua source module is {} bytes; max is {} bytes.",
                        request.module_bytes.len(),
                        MAX_MODULE_BYTES
                    ),
                )],
            );
        }
        let source = match std::str::from_utf8(&request.module_bytes) {
            Ok(source) => source,
            Err(error) => {
                return failed_load(
                    request.module_ref,
                    vec![ScriptDiagnostic::error(
                        "LUA_SCRIPTING_MODULE_NOT_UTF8",
                        format!("Lua 5.4 source module is not UTF-8: {error}"),
                    )],
                )
            }
        };

        let module_key = normalized_module_key(&request.module_ref);
        if let Some(old) = self.modules.remove(&module_key) {
            let _ = self.lua.remove_registry_value(old.exports);
        }

        match self.compile_module(&request.module_ref, source) {
            Ok(exports) => {
                self.load_count = self.load_count.saturating_add(1);
                let mut metadata = request.metadata;
                metadata.insert("provider".to_owned(), PROVIDER_LABEL.to_owned());
                metadata.insert("language".to_owned(), "lua".to_owned());
                let record = ScriptingModuleRecord {
                    module_ref: request.module_ref,
                    state: ScriptModuleState::Loaded,
                    permissions: request.permissions,
                    module_bytes_len: source.len() as u64,
                    metadata,
                    diagnostics: vec![ScriptDiagnostic::info(
                        "LUA_SCRIPTING_MODULE_LOADED",
                        "Lua module compiled and its export table was cached.",
                    )],
                    ..ScriptingModuleRecord::default()
                };
                self.modules.insert(
                    module_key,
                    LoadedLuaModule {
                        record: record.clone(),
                        exports,
                    },
                );
                ScriptingModuleLoadBytesResponse {
                    ok: true,
                    module: record,
                    diagnostics: Vec::new(),
                }
            }
            Err(error) => {
                self.error_count = self.error_count.saturating_add(1);
                failed_load(
                    request.module_ref,
                    vec![ScriptDiagnostic::error(
                        "LUA_SCRIPTING_COMPILE_FAILED",
                        error,
                    )],
                )
            }
        }
    }

    fn compile_module(
        &self,
        module_ref: &ScriptModuleRef,
        source: &str,
    ) -> Result<RegistryKey, String> {
        let env = self.lua.create_table().map_err(|e| e.to_string())?;
        let metatable = self.lua.create_table().map_err(|e| e.to_string())?;
        metatable
            .set("__index", self.lua.globals())
            .map_err(|e| e.to_string())?;
        env.set_metatable(Some(metatable))
            .map_err(|e| e.to_string())?;

        self.arm_instruction_budget();
        let value: Value = self
            .lua
            .load(source)
            .set_name(module_ref.reference.as_str())
            .set_environment(env.clone())
            .eval()
            .map_err(|e| format!("{}: {e}", module_ref.reference))?;

        let exports = match value {
            Value::Table(table) => table,
            Value::Nil => env,
            _ => {
                return Err(format!(
                "{}: module must return an export table (or nil to export its module environment)",
                module_ref.reference
            ))
            }
        };
        self.lua
            .create_registry_value(exports)
            .map_err(|e| e.to_string())
    }

    pub fn unload_module(
        &mut self,
        module_ref: ScriptModuleRef,
    ) -> ScriptingModuleLoadBytesResponse {
        let key = normalized_module_key(&module_ref);
        match self.modules.remove(&key) {
            Some(mut loaded) => {
                let _ = self.lua.remove_registry_value(loaded.exports);
                loaded.record.state = ScriptModuleState::Disabled;
                ScriptingModuleLoadBytesResponse {
                    ok: true,
                    module: loaded.record,
                    diagnostics: Vec::new(),
                }
            }
            None => failed_load(
                module_ref,
                vec![ScriptDiagnostic::warning(
                    "LUA_SCRIPTING_MODULE_NOT_LOADED",
                    "Requested Lua module is not loaded.",
                )],
            ),
        }
    }

    pub fn invoke_bytes(&mut self, request: ScriptingRequestBytes) -> ScriptingResponseBytes {
        self.request_count = self.request_count.saturating_add(1);
        self.invoke_impl(request)
    }

    pub fn frame_bytes(&mut self, request: ScriptingRequestBytes) -> ScriptingResponseBytes {
        self.frame_count = self.frame_count.saturating_add(1);
        self.invoke_impl(request)
    }

    fn invoke_impl(&mut self, request: ScriptingRequestBytes) -> ScriptingResponseBytes {
        let mut response = ScriptingResponseBytes::empty_for(&request);
        if request.operation.trim().is_empty() {
            response.status = ScriptingResponseStatus::InvalidRequest;
            response.diagnostics.push(ScriptDiagnostic::error(
                "LUA_SCRIPTING_EMPTY_OPERATION",
                "Scripting request operation must name an exported Lua function.",
            ));
            return response;
        }
        let module_ref = ScriptModuleRef::new(request.script_ref.clone());
        let key = normalized_module_key(&module_ref);
        let Some(loaded) = self.modules.get(&key) else {
            response.status = ScriptingResponseStatus::Rejected;
            response.diagnostics.push(ScriptDiagnostic::error(
                "LUA_SCRIPTING_MODULE_NOT_LOADED",
                format!("Lua module '{}' is not loaded.", request.script_ref),
            ));
            return response;
        };

        let result = self.call_export(loaded, &request);
        match result {
            Ok(payload) => {
                if payload.len() > MAX_RESPONSE_BYTES {
                    response.status = ScriptingResponseStatus::ProviderError;
                    response.diagnostics.push(ScriptDiagnostic::error(
                        "LUA_SCRIPTING_RESPONSE_TOO_LARGE",
                        format!("Lua response exceeded {} bytes.", MAX_RESPONSE_BYTES),
                    ));
                } else {
                    response.status = ScriptingResponseStatus::Ok;
                    response.payload_bytes = payload;
                    response
                        .metadata
                        .insert("provider".to_owned(), PROVIDER_LABEL.to_owned());
                }
            }
            Err(error) => {
                self.error_count = self.error_count.saturating_add(1);
                response.status = ScriptingResponseStatus::ProviderError;
                response.diagnostics.push(ScriptDiagnostic::error(
                    "LUA_SCRIPTING_INVOKE_FAILED",
                    error,
                ));
            }
        }
        response
    }

    fn call_export(
        &self,
        loaded: &LoadedLuaModule,
        request: &ScriptingRequestBytes,
    ) -> Result<Vec<u8>, String> {
        let exports: Table = self
            .lua
            .registry_value(&loaded.exports)
            .map_err(|e| e.to_string())?;
        let function: Function = exports
            .get(request.operation.as_str())
            .map_err(|e| format!("export '{}': {e}", request.operation))?;

        let payload = if request
            .metadata
            .get("payload_format")
            .is_some_and(|value| value.eq_ignore_ascii_case("json"))
        {
            crate::json_value::decode_json_compatible_value(&self.lua, &request.payload_bytes)?
        } else {
            Value::String(
                self.lua
                    .create_string(&request.payload_bytes)
                    .map_err(|e| e.to_string())?,
            )
        };
        let context = self
            .lua
            .create_string(&request.context_bytes)
            .map_err(|e| e.to_string())?;
        let metadata = self.lua.create_table().map_err(|e| e.to_string())?;
        for (key, value) in &request.metadata {
            metadata
                .set(key.as_str(), value.as_str())
                .map_err(|e| e.to_string())?;
        }
        self.arm_instruction_budget();
        let result: Value = function
            .call((payload, context, metadata))
            .map_err(|e| format!("{}::{}: {e}", request.script_ref, request.operation))?;
        match result {
            Value::Nil => Ok(Vec::new()),
            Value::String(value) => Ok(value.as_bytes().to_vec()),
            Value::Boolean(_) | Value::Integer(_) | Value::Number(_) | Value::Table(_) => {
                crate::json_value::encode_json_compatible_value(result)
            }
            other => Err(format!(
                "{}::{} returned unsupported Lua type '{}'; expected nil, string, or JSON-compatible structured value",
                request.script_ref,
                request.operation,
                other.type_name()
            )),
        }
    }

    #[inline]
    fn arm_instruction_budget(&self) {
        self.instruction_budget
            .store(MAX_INSTRUCTIONS_PER_CALL, Ordering::Relaxed);
    }

    pub fn dump_state(&self) -> ScriptingStateDump {
        let counters = BTreeMap::from([
            ("requests_processed".to_owned(), self.request_count),
            ("frames_processed".to_owned(), self.frame_count),
            ("modules_loaded_total".to_owned(), self.load_count),
            ("provider_errors".to_owned(), self.error_count),
        ]);
        let mut provider_metadata = BTreeMap::new();
        provider_metadata.insert("language".to_owned(), "lua".to_owned());
        provider_metadata.insert("runtime".to_owned(), "lua-5.4".to_owned());
        ScriptingStateDump {
            gateway: ENGINE_SCRIPTING_SERVICE_ID.to_owned(),
            provider: LUA_PROVIDER_SERVICE_ID.to_owned(),
            backend_capability: SCRIPTING_BACKEND_CAPABILITY_ID.to_owned(),
            backend: PROVIDER_LABEL.to_owned(),
            loaded_modules: self.modules.values().map(|m| m.record.clone()).collect(),
            counters,
            notes: vec![
                "One .ysc asset is one script module; .ysc is not a dictionary and has no @entry selector.".to_owned(),
                "Lua modules return an export table. Engine request.operation selects an export function.".to_owned(),
                "Requests marked payload_format=json arrive as JSON-compatible Lua values; structured Lua results are returned as JSON bytes.".to_owned(),
                "No direct ECS/world bindings are exposed by the provider.".to_owned(),
            ],
            provider_metadata,
            ..ScriptingStateDump::default()
        }
    }
}

fn failed_load(
    module_ref: ScriptModuleRef,
    diagnostics: Vec<ScriptDiagnostic>,
) -> ScriptingModuleLoadBytesResponse {
    ScriptingModuleLoadBytesResponse {
        ok: false,
        module: ScriptingModuleRecord {
            module_ref,
            state: ScriptModuleState::Failed,
            diagnostics: diagnostics.clone(),
            ..ScriptingModuleRecord::default()
        },
        diagnostics,
    }
}

fn normalized_module_key(module_ref: &ScriptModuleRef) -> String {
    if !module_ref.module_id.trim().is_empty() {
        module_ref.module_id.trim().to_ascii_lowercase()
    } else {
        module_ref.reference.trim().to_ascii_lowercase()
    }
}

static RUNTIME: OnceLock<Mutex<Option<LuaRuntimeState>>> = OnceLock::new();

fn runtime_cell() -> &'static Mutex<Option<LuaRuntimeState>> {
    RUNTIME.get_or_init(|| Mutex::new(None))
}

pub fn reset_runtime() -> Result<(), String> {
    let runtime = LuaRuntimeState::new()?;
    *runtime_cell().lock() = Some(runtime);
    Ok(())
}

pub fn shutdown_runtime() {
    *runtime_cell().lock() = None;
}

pub fn with_runtime<R>(f: impl FnOnce(&mut LuaRuntimeState) -> R) -> Result<R, String> {
    let mut guard = runtime_cell().lock();
    if guard.is_none() {
        *guard = Some(LuaRuntimeState::new()?);
    }
    let runtime = guard
        .as_mut()
        .ok_or_else(|| "Lua scripting runtime is unavailable".to_owned())?;
    Ok(f(runtime))
}

#[cfg(test)]
mod tests {
    use super::*;
    use newengine_scripting_api::{ScriptingModuleLoadBytesRequest, ScriptingRequestBytes};

    fn module(source: &str) -> ScriptingModuleLoadBytesRequest {
        ScriptingModuleLoadBytesRequest {
            module_ref: ScriptModuleRef::new("scripts/test.ysc"),
            module_bytes: source.as_bytes().to_vec(),
            ..ScriptingModuleLoadBytesRequest::default()
        }
    }

    fn fps_gameplay_module() -> ScriptingModuleLoadBytesRequest {
        let source = r#"
return {
  on_hit = function(payload)
    return {
      allow_default = false,
      commands = {
        schema = "newengine.gameplay.command_buffer.v1",
        version = 1,
        atomic = true,
        commands = {
          { kind = "deal_damage", target = payload.target, amount = payload.base_damage, damage_type = "test" },
          { kind = "play_effect", effect = "test.hit", entity = payload.target, position = payload.point }
        }
      }
    }
  end,
  action = function(payload)
    return {
      schema = "newengine.gameplay.command_buffer.v1",
      version = 1,
      atomic = true,
      commands = {
        { kind = "give_item", target = payload.actor, item = "consumable.medkit.standard", quantity = 1 }
      }
    }
  end,
  ability = function(payload)
    return {
      schema = "newengine.gameplay.command_buffer.v1",
      version = 1,
      atomic = true,
      commands = {
        { kind = "spawn_archetype", archetype = "test.probe", position = payload.origin }
      }
    }
  end,
  state_machine_step = function(payload)
    return {
      next_state = "active",
      variables = payload.variables or {},
      commands = {
        schema = "newengine.gameplay.command_buffer.v1",
        version = 1,
        atomic = true,
        commands = {
          { kind = "set_objective", objective = "test.objective", state = "active" }
        }
      }
    }
  end
}
"#;
        ScriptingModuleLoadBytesRequest {
            module_ref: ScriptModuleRef::new("scripts/fps_gameplay.ysc"),
            module_bytes: source.as_bytes().to_vec(),
            ..ScriptingModuleLoadBytesRequest::default()
        }
    }

    #[test]
    fn module_ref_is_selectorless_ysc() {
        assert!(LuaRuntimeState::validate_module_ref(ScriptModuleRef::new("scripts/test.ysc")).ok);
        assert!(
            !LuaRuntimeState::validate_module_ref(ScriptModuleRef::new("scripts/test.ysc@entry"))
                .ok
        );
    }

    #[test]
    fn module_exports_are_invoked_by_operation() {
        let mut runtime = LuaRuntimeState::new().unwrap();
        assert!(
            runtime
                .load_module_bytes(module(
                    "return { echo = function(payload) return payload end }"
                ))
                .ok
        );
        let response = runtime.invoke_bytes(ScriptingRequestBytes {
            request_id: "r1".to_owned(),
            script_ref: "scripts/test.ysc".to_owned(),
            operation: "echo".to_owned(),
            payload_bytes: b"hello".to_vec(),
            ..ScriptingRequestBytes::default()
        });
        assert_eq!(response.status, ScriptingResponseStatus::Ok);
        assert_eq!(response.payload_bytes, b"hello");
    }

    #[test]
    fn structured_json_request_arrives_as_lua_table() {
        let mut runtime = LuaRuntimeState::new().unwrap();
        assert!(runtime
            .load_module_bytes(module(
                "return { inspect = function(payload) return { speed = payload.player.speed, enabled = payload.enabled } end }"
            ))
            .ok);
        let response = runtime.invoke_bytes(ScriptingRequestBytes {
            request_id: "r-json".to_owned(),
            script_ref: "scripts/test.ysc".to_owned(),
            operation: "inspect".to_owned(),
            payload_bytes: br#"{"player":{"speed":7.3},"enabled":true}"#.to_vec(),
            metadata: BTreeMap::from([("payload_format".to_owned(), "json".to_owned())]),
            ..ScriptingRequestBytes::default()
        });
        assert_eq!(response.status, ScriptingResponseStatus::Ok);
        let value: serde_json::Value = serde_json::from_slice(&response.payload_bytes).unwrap();
        assert_eq!(value["speed"], 7.3);
        assert_eq!(value["enabled"], true);
    }

    #[test]
    fn production_fps_gameplay_exports_transactional_commands() {
        let mut runtime = LuaRuntimeState::new().unwrap();
        assert!(runtime.load_module_bytes(fps_gameplay_module()).ok);

        let invoke_json =
            |runtime: &mut LuaRuntimeState, operation: &str, payload: serde_json::Value| {
                let response = runtime.invoke_bytes(ScriptingRequestBytes {
                    request_id: format!("prod-{operation}"),
                    script_ref: "scripts/fps_gameplay.ysc".to_owned(),
                    operation: operation.to_owned(),
                    payload_bytes: serde_json::to_vec(&payload).unwrap(),
                    metadata: BTreeMap::from([("payload_format".to_owned(), "json".to_owned())]),
                    ..ScriptingRequestBytes::default()
                });
                assert_eq!(
                    response.status,
                    ScriptingResponseStatus::Ok,
                    "{operation}: {:?}",
                    response.diagnostics
                );
                serde_json::from_slice::<serde_json::Value>(&response.payload_bytes).unwrap()
            };

        let hit = invoke_json(
            &mut runtime,
            "on_hit",
            serde_json::json!({
                "kind": "hit",
                "shooter": 11,
                "target": 22,
                "shot_sequence": 3,
                "base_damage": 25.0,
                "fixed_tick": 7,
                "point": [0.0, 0.0, 0.0],
                "normal": [0.0, 1.0, 0.0]
            }),
        );
        assert_eq!(hit["allow_default"], false);
        assert_eq!(
            hit["commands"]["schema"],
            "newengine.gameplay.command_buffer.v1"
        );
        assert_eq!(hit["commands"]["commands"][0]["kind"], "deal_damage");
        assert_eq!(hit["commands"]["commands"][1]["kind"], "play_effect");

        let action = invoke_json(
            &mut runtime,
            "action",
            serde_json::json!({
                "action": "action.grant_medkit",
                "actor": 11,
                "context": null
            }),
        );
        assert_eq!(action["commands"][0]["kind"], "give_item");

        let ability = invoke_json(
            &mut runtime,
            "ability",
            serde_json::json!({
                "ability": "ability.spawn_probe",
                "actor": 11,
                "origin": [1.0, 2.0, 3.0],
                "context": null
            }),
        );
        assert_eq!(ability["commands"][0]["kind"], "spawn_archetype");

        let machine = invoke_json(
            &mut runtime,
            "state_machine_step",
            serde_json::json!({
                "machine": "mission.extraction",
                "state": "idle",
                "event": "activate",
                "context": null,
                "variables": { }
            }),
        );
        assert_eq!(machine["next_state"], "active");
        assert_eq!(machine["commands"]["commands"][0]["kind"], "set_objective");
    }

    #[test]
    fn module_without_export_is_rejected() {
        let mut runtime = LuaRuntimeState::new().unwrap();
        assert!(
            runtime
                .load_module_bytes(module("return { okay = function() return 'x' end }"))
                .ok
        );
        let response = runtime.invoke_bytes(ScriptingRequestBytes {
            request_id: "missing".to_owned(),
            script_ref: "scripts/test.ysc".to_owned(),
            operation: "missing".to_owned(),
            ..ScriptingRequestBytes::default()
        });
        assert_eq!(response.status, ScriptingResponseStatus::ProviderError);
    }

    #[test]
    fn runaway_lua_is_stopped_by_instruction_budget() {
        let mut runtime = LuaRuntimeState::new().unwrap();
        assert!(
            runtime
                .load_module_bytes(module("return { spin = function() while true do end end }"))
                .ok
        );
        let response = runtime.invoke_bytes(ScriptingRequestBytes {
            request_id: "spin".to_owned(),
            script_ref: "scripts/test.ysc".to_owned(),
            operation: "spin".to_owned(),
            ..ScriptingRequestBytes::default()
        });
        assert_eq!(response.status, ScriptingResponseStatus::ProviderError);
        assert!(response
            .diagnostics
            .iter()
            .any(|diag| diag.message.contains("instruction budget")));
    }
}
