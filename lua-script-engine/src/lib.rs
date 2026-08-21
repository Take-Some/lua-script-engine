#![forbid(unsafe_op_in_unsafe_fn)]

mod json_value;
mod plugin;
mod runtime;
mod service;

use abi_stable::sabi_trait::TD_Opaque;
use abi_stable::std_types::RString;
use newengine_plugin_api::{
    export_plugin_root, PluginBootstrapPhase, PluginKind, PluginModuleDyn, PluginModule_TO,
    PluginSignatureV1, PluginUiAssetsV1,
};

use plugin::{LuaScriptingPlugin, LUA_PLUGIN_ID, LUA_PLUGIN_NAME};

export_plugin_root!(create_module, ui_assets_v1);

extern "C" fn create_module() -> PluginModuleDyn<'static> {
    PluginModule_TO::from_value(LuaScriptingPlugin, TD_Opaque)
}

extern "C" fn ui_assets_v1() -> PluginUiAssetsV1 {
    PluginUiAssetsV1::empty()
}

#[no_mangle]
pub extern "C" fn newengine_plugin_signature_v1() -> PluginSignatureV1 {
    PluginSignatureV1 {
        id: RString::from(LUA_PLUGIN_ID),
        name: RString::from(LUA_PLUGIN_NAME),
        version: RString::from(env!("CARGO_PKG_VERSION")),
        kind: PluginKind::Runtime,
        bootstrap_phase: PluginBootstrapPhase::Bootstrap,
    }
}
