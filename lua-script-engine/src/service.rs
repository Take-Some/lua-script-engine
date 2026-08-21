use abi_stable::std_types::{RResult, RString, RVec};
use abi_stable::StableAbi;
use newengine_plugin_api::prelude::*;
use newengine_scripting_api::{
    decode_scripting_module_load_bytes_request, decode_scripting_request_bytes,
    encode_scripting_module_load_bytes_response, encode_scripting_response_bytes, ScriptModuleRef,
    ScriptingInvokeEnvelope, ScriptingModuleUnloadRequest, SCRIPTING_BINARY_PROTOCOL_ID,
    SCRIPTING_SERVICE_METHOD_BINDING_MANIFEST_JSON_V1, SCRIPTING_SERVICE_METHOD_DUMP_STATE_JSON_V1,
    SCRIPTING_SERVICE_METHOD_FRAME_BYTES_V1, SCRIPTING_SERVICE_METHOD_INFO,
    SCRIPTING_SERVICE_METHOD_INVOKE, SCRIPTING_SERVICE_METHOD_INVOKE_BYTES_V1,
    SCRIPTING_SERVICE_METHOD_LOAD_MODULE_BYTES_V1, SCRIPTING_SERVICE_METHOD_SHUTDOWN_V1,
    SCRIPTING_SERVICE_METHOD_UNLOAD_MODULE_JSON_V1,
    SCRIPTING_SERVICE_METHOD_VALIDATE_MODULE_REF_JSON_V1,
};

use crate::plugin::{LUA_PLUGIN_ID, LUA_PROVIDER_SERVICE_ID};

use crate::runtime::{shutdown_runtime, with_runtime, LuaRuntimeState};

#[derive(StableAbi)]
#[repr(C)]
pub struct LuaScriptingService;

impl LuaScriptingService {
    fn info_json() -> Result<Vec<u8>, String> {
        with_runtime(|runtime| serde_json::to_vec(&runtime.service_info()))?
            .map_err(|e| e.to_string())
    }

    fn invoke_binary(payload: Blob, frame: bool) -> RResult<Blob, RString> {
        let request = match decode_scripting_request_bytes(payload.as_slice()) {
            Ok(request) => request,
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        let response = match with_runtime(|runtime| {
            if frame {
                runtime.frame_bytes(request)
            } else {
                runtime.invoke_bytes(request)
            }
        }) {
            Ok(response) => response,
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        RResult::ROk(RVec::from(encode_scripting_response_bytes(&response)))
    }

    fn load_module(payload: Blob) -> RResult<Blob, RString> {
        let request = match decode_scripting_module_load_bytes_request(payload.as_slice()) {
            Ok(request) => request,
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        let response = match with_runtime(|runtime| runtime.load_module_bytes(request)) {
            Ok(response) => response,
            Err(error) => return RResult::RErr(RString::from(error.to_string())),
        };
        RResult::ROk(RVec::from(encode_scripting_module_load_bytes_response(
            &response,
        )))
    }

    fn json_response<T: serde::Serialize>(value: &T) -> RResult<Blob, RString> {
        match serde_json::to_vec(value) {
            Ok(bytes) => RResult::ROk(RVec::from(bytes)),
            Err(error) => RResult::RErr(RString::from(error.to_string())),
        }
    }

    fn control_invoke(payload: Blob) -> RResult<Blob, RString> {
        let envelope = match serde_json::from_slice::<ScriptingInvokeEnvelope>(payload.as_slice()) {
            Ok(envelope) => envelope,
            Err(error) => {
                return RResult::RErr(RString::from(format!(
                    "lua scripting: invalid invoke envelope: {error}"
                )))
            }
        };
        let inner = RVec::from(envelope.request_bytes);
        match envelope.method.as_str() {
            SCRIPTING_SERVICE_METHOD_LOAD_MODULE_BYTES_V1 => Self::load_module(inner),
            SCRIPTING_SERVICE_METHOD_INVOKE_BYTES_V1 => Self::invoke_binary(inner, false),
            SCRIPTING_SERVICE_METHOD_FRAME_BYTES_V1 => Self::invoke_binary(inner, true),
            SCRIPTING_SERVICE_METHOD_DUMP_STATE_JSON_V1 => match with_runtime(|r| r.dump_state()) {
                Ok(value) => Self::json_response(&value),
                Err(error) => RResult::RErr(RString::from(error.to_string())),
            },
            other => RResult::RErr(RString::from(format!(
                "lua scripting: unsupported control method '{other}'"
            ))),
        }
    }
}

impl ServiceV1 for LuaScriptingService {
    fn id(&self) -> RString {
        RString::from(LUA_PROVIDER_SERVICE_ID)
    }

    fn describe(&self) -> RString {
        RString::from(
            serde_json::json!({
                "schema": "newengine.service_description.v1",
                "id": LUA_PROVIDER_SERVICE_ID,
                "provider": LUA_PLUGIN_ID,
                "protocol": SCRIPTING_BINARY_PROTOCOL_ID,
                "backend": "lua54",
                "features": [
                    "ysc-module-bytes",
                    "compiled-module-cache",
                    "module-export-table",
                    "no-direct-world-access"
                ]
            })
            .to_string(),
        )
    }

    fn call(&self, method: MethodName, payload: Blob) -> RResult<Blob, RString> {
        match method.as_str() {
            SCRIPTING_SERVICE_METHOD_INFO => match Self::info_json() {
                Ok(bytes) => RResult::ROk(RVec::from(bytes)),
                Err(error) => RResult::RErr(RString::from(error.to_string())),
            },
            SCRIPTING_SERVICE_METHOD_LOAD_MODULE_BYTES_V1 => Self::load_module(payload),
            SCRIPTING_SERVICE_METHOD_INVOKE_BYTES_V1 => Self::invoke_binary(payload, false),
            SCRIPTING_SERVICE_METHOD_FRAME_BYTES_V1 => Self::invoke_binary(payload, true),
            SCRIPTING_SERVICE_METHOD_INVOKE => Self::control_invoke(payload),
            SCRIPTING_SERVICE_METHOD_DUMP_STATE_JSON_V1 => match with_runtime(|r| r.dump_state()) {
                Ok(value) => Self::json_response(&value),
                Err(error) => RResult::RErr(RString::from(error.to_string())),
            },
            SCRIPTING_SERVICE_METHOD_VALIDATE_MODULE_REF_JSON_V1 => {
                let module_ref = match serde_json::from_slice::<ScriptModuleRef>(payload.as_slice())
                {
                    Ok(value) => value,
                    Err(error) => return RResult::RErr(RString::from(error.to_string())),
                };
                Self::json_response(&LuaRuntimeState::validate_module_ref(module_ref))
            }
            SCRIPTING_SERVICE_METHOD_UNLOAD_MODULE_JSON_V1 => {
                let request = match serde_json::from_slice::<ScriptingModuleUnloadRequest>(
                    payload.as_slice(),
                ) {
                    Ok(value) => value,
                    Err(error) => return RResult::RErr(RString::from(error.to_string())),
                };
                match with_runtime(|r| r.unload_module(request.module_ref)) {
                    Ok(value) => Self::json_response(&value),
                    Err(error) => RResult::RErr(RString::from(error.to_string())),
                }
            }
            SCRIPTING_SERVICE_METHOD_BINDING_MANIFEST_JSON_V1 => {
                Self::json_response(&serde_json::json!({
                    "schema": "newengine.scripting.lua.binding_manifest.v1",
                    "provider": "engine.scripting.lua",
                    "language": "lua",
                    "bindings": [],
                    "policy": "v0.1 exposes no direct engine bindings; bindings must be explicit and capability-scoped"
                }))
            }
            SCRIPTING_SERVICE_METHOD_SHUTDOWN_V1 => {
                shutdown_runtime();
                RResult::ROk(RVec::new())
            }
            _ => RResult::RErr(RString::from(format!(
                "lua scripting: unknown method '{}'",
                method
            ))),
        }
    }
}
