use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct IntValue(pub i64);
impl<'de> Deserialize<'de> for IntValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        match v {
            Value::Null => Ok(Self(0)),
            Value::Number(n) => Ok(Self(
                n.as_i64()
                    .unwrap_or_else(|| n.as_f64().unwrap_or(0.) as i64),
            )),
            Value::String(s) => s.parse::<i64>().map(Self).map_err(serde::de::Error::custom),
            _ => Err(serde::de::Error::custom(
                "expected integer or integer string",
            )),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StringValue(pub String);
impl<'de> Deserialize<'de> for StringValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        match v {
            Value::Null => Ok(Self(String::new())),
            Value::Number(n) => Ok(Self(n.to_string())),
            Value::String(s) => Ok(Self(s)),
            _ => Err(serde::de::Error::custom("expected string or number")),
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoolValue(pub bool);
impl<'de> Deserialize<'de> for BoolValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        match v {
            Value::Null => Ok(Self(false)),
            Value::Bool(b) => Ok(Self(b)),
            Value::String(s) if s == "true" || s == "false" => Ok(Self(s == "true")),
            _ => Err(serde::de::Error::custom(
                "expected boolean or boolean string",
            )),
        }
    }
}
