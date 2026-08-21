# LuaScriptEngine

First-party `engine.scripting` provider backed by sandboxed Lua 5.4.

## Runtime contract

- one `.ysc` asset is one opaque script module; `.ysc` is **not a dictionary** and uses no `@entry` selector;
- the AssetManager/engine forwards the decoded `.ysc` body bytes to `scripting.load_module_bytes_v1`;
- the Lua provider compiles the module once and caches its export table;
- a module returns a Lua table whose keys are operation names;
- `scripting.invoke_bytes_v1` / `scripting.frame_bytes_v1` call the requested export;
- payload/context are binary Lua strings; a return value is either `nil` or a binary Lua string;
- no direct ECS/world access is exposed in v0.1.0. Engine bindings will be added through explicit capability/binding layers later.

Example module body:

```lua
return {
  ping = function(payload, context, metadata)
    return "pong:" .. payload
  end,

  update = function(payload, context, metadata)
    return nil
  end,
}
```
