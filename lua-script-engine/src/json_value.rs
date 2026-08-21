use mlua::{Lua, Table, Value};
use serde_json::{Map, Number};

pub(crate) fn encode_json_compatible_value(value: Value) -> Result<Vec<u8>, String> {
    let value = to_json_value(value, 0)?;
    serde_json::to_vec(&value)
        .map_err(|error| format!("Lua structured result JSON encode failed: {error}"))
}

pub(crate) fn decode_json_compatible_value(lua: &Lua, bytes: &[u8]) -> Result<Value, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Lua structured request JSON decode failed: {error}"))?;
    from_json_value(lua, value, 0)
}

fn from_json_value(lua: &Lua, value: serde_json::Value, depth: usize) -> Result<Value, String> {
    if depth > 64 {
        return Err("Lua structured request exceeded maximum nesting depth 64".to_owned());
    }
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(value) => Ok(Value::Boolean(value)),
        serde_json::Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(Value::Integer(integer))
            } else if let Some(unsigned) = value.as_u64() {
                let integer = i64::try_from(unsigned).map_err(|_| {
                    format!(
                        "Lua structured request integer {unsigned} exceeds signed Lua integer range"
                    )
                })?;
                Ok(Value::Integer(integer))
            } else {
                let number = value
                    .as_f64()
                    .filter(|number| number.is_finite())
                    .ok_or_else(|| {
                        "Lua structured request contains non-finite number".to_owned()
                    })?;
                Ok(Value::Number(number))
            }
        }
        serde_json::Value::String(value) => lua
            .create_string(&value)
            .map(Value::String)
            .map_err(|error| format!("Lua structured request string allocation failed: {error}")),
        serde_json::Value::Array(values) => {
            let table = lua.create_table().map_err(|error| {
                format!("Lua structured request array allocation failed: {error}")
            })?;
            for (index, value) in values.into_iter().enumerate() {
                table
                    .set(index + 1, from_json_value(lua, value, depth + 1)?)
                    .map_err(|error| {
                        format!("Lua structured request array insert failed: {error}")
                    })?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(values) => {
            let table = lua.create_table().map_err(|error| {
                format!("Lua structured request object allocation failed: {error}")
            })?;
            for (key, value) in values {
                table
                    .set(key, from_json_value(lua, value, depth + 1)?)
                    .map_err(|error| {
                        format!("Lua structured request object insert failed: {error}")
                    })?;
            }
            Ok(Value::Table(table))
        }
    }
}

fn to_json_value(value: Value, depth: usize) -> Result<serde_json::Value, String> {
    if depth > 64 {
        return Err("Lua structured result exceeded maximum nesting depth 64".to_owned());
    }
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(value) => Ok(serde_json::Value::Bool(value)),
        Value::Integer(value) => Ok(serde_json::Value::Number(Number::from(value))),
        Value::Number(value) => Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| "Lua structured result contains non-finite number".to_owned()),
        Value::String(value) => value
            .to_str()
            .map(|text| serde_json::Value::String(text.to_owned()))
            .map_err(|error| format!("Lua structured result contains non-UTF8 string: {error}")),
        Value::Table(table) => table_to_json(table, depth + 1),
        other => Err(format!(
            "Lua structured result contains unsupported type '{}'",
            other.type_name()
        )),
    }
}

fn table_to_json(table: Table, depth: usize) -> Result<serde_json::Value, String> {
    let mut integer_entries = Vec::<(i64, Value)>::new();
    let mut string_entries = Vec::<(String, Value)>::new();

    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair.map_err(|error| format!("Lua table iteration failed: {error}"))?;
        match key {
            Value::Integer(index) if index >= 1 => integer_entries.push((index, value)),
            Value::String(key) => {
                let key = key
                    .to_str()
                    .map_err(|error| format!("Lua table key is not UTF-8: {error}"))?
                    .to_owned();
                string_entries.push((key, value));
            }
            other => {
                return Err(format!(
                    "Lua structured table key type '{}' is unsupported; use string keys or dense 1-based arrays",
                    other.type_name()
                ))
            }
        }
    }

    if string_entries.is_empty() {
        integer_entries.sort_by_key(|(index, _)| *index);
        let dense = integer_entries
            .iter()
            .enumerate()
            .all(|(offset, (index, _))| *index == (offset as i64) + 1);
        if dense {
            let mut values = Vec::with_capacity(integer_entries.len());
            for (_, value) in integer_entries {
                values.push(to_json_value(value, depth)?);
            }
            return Ok(serde_json::Value::Array(values));
        }
    }

    if !integer_entries.is_empty() {
        return Err(
            "Lua structured table mixes object keys with integer keys or has a sparse array; use either a string-keyed object or a dense 1-based array"
                .to_owned(),
        );
    }

    let mut object = Map::with_capacity(string_entries.len());
    for (key, value) in string_entries {
        object.insert(key, to_json_value(value, depth)?);
    }
    Ok(serde_json::Value::Object(object))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::Lua;

    #[test]
    fn dense_lua_array_becomes_json_array() {
        let lua = Lua::new();
        let value: Value = lua.load("return {1, 2, 3}").eval().unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(
                &encode_json_compatible_value(value).unwrap()
            )
            .unwrap(),
            serde_json::json!([1, 2, 3])
        );
    }

    #[test]
    fn string_keyed_lua_table_becomes_json_object() {
        let lua = Lua::new();
        let value: Value = lua
            .load("return { player = { speed = 7.3 }, enabled = true }")
            .eval()
            .unwrap();
        let json = serde_json::from_slice::<serde_json::Value>(
            &encode_json_compatible_value(value).unwrap(),
        )
        .unwrap();
        assert_eq!(json["player"]["speed"], 7.3);
        assert_eq!(json["enabled"], true);
    }

    #[test]
    fn json_object_becomes_lua_table() {
        let lua = Lua::new();
        let value = decode_json_compatible_value(
            &lua,
            br#"{"player":{"speed":7.3},"enabled":true,"slots":[1,2]}"#,
        )
        .unwrap();
        let table = match value {
            Value::Table(table) => table,
            other => panic!("expected table, got {}", other.type_name()),
        };
        assert!(table.get::<bool>("enabled").unwrap());
        let player = table.get::<Table>("player").unwrap();
        assert_eq!(player.get::<f64>("speed").unwrap(), 7.3);
        let slots = table.get::<Table>("slots").unwrap();
        assert_eq!(slots.get::<i64>(1).unwrap(), 1);
        assert_eq!(slots.get::<i64>(2).unwrap(), 2);
    }
}
