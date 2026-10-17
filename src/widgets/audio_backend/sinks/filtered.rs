//! Keep the fields used for sink discovery without allocating parameter trees.
use serde::{
    Deserialize, Deserializer,
    de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::fmt;

pub(super) struct DumpObject(pub(super) Value);

#[derive(Clone, Copy)]
enum Fields {
    Object,
    Info,
    Properties,
}

impl<'de> Deserialize<'de> for DumpObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Fields::Object.deserialize(deserializer).map(Self)
    }
}

impl<'de> DeserializeSeed<'de> for Fields {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Fields {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a PipeWire graph object or node info")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut fields: M) -> Result<Value, M::Error> {
        let mut object = Map::new();
        while let Some(key) = fields.next_key::<String>()? {
            let value = match (self, key.as_str()) {
                (Self::Object, "info") => fields.next_value_seed(Self::Info)?,
                (Self::Object | Self::Info, "props") => fields.next_value_seed(Self::Properties)?,
                (Self::Object, "id" | "type" | "metadata")
                | (
                    Self::Properties,
                    "metadata.name" | "media.class" | "node.name" | "node.description"
                    | "node.nick" | "device.description",
                ) => fields.next_value::<Value>()?,
                _ => {
                    fields.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            object.insert(key, value);
        }
        Ok(Value::Object(object))
    }

    // The old Value parser accepted irrelevant non-object entries. Preserve
    // that behavior, including malformed info types, without keeping them.
    fn visit_seq<S: SeqAccess<'de>>(self, mut values: S) -> Result<Value, S::Error> {
        while values.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Value::Null)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_str<E>(self, _: &str) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
}
